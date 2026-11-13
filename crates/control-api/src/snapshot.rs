//! `POST /v1/runs/{runId}/snapshot/download-url`: a short-lived download URL for the latest snapshot bundle of a run.
//!
//! agentd saves `snapshot.bundle` (a git bundle of `refs/heads/snapshot`) and then `snapshot.json` under
//! `runs/<run>/agents/direct/turns/1/` about every 10 minutes while the files change. This route (owner only) reads the record (at most
//! 64 KiB) and the first 8 KiB of the bundle, checks that both name the same commit, and presigns the bundle for fifteen minutes. The
//! answer carries the bundle's size and ETag, so the client can ask for the object only while it is still the one that was checked.
//!
//! The worker role can write both objects, so the pair is worker-made: it is checked for consistency and shape, not for who wrote it
//! (`ham_core::snapshot::SNAPSHOT_TRUST_NOTE`). The role of this Lambda may get exactly these two keys (`infra`, statement
//! `SnapshotDownload`). The presigned URL is in the answer and nowhere else: it is in no diagnostic, and the access line holds the path only.

use crate::handler::{
    Blobs, DOWNLOAD_URL_TTL, Deps, Execution, ObjectPrefix, Reply, Store, error, json, not_found,
    owned, unavailable,
};
use crate::ids::now_rfc3339;
use ham_core::snapshot::{
    HEADER_BYTES, MAX_RECORD_BYTES, STALE_RECORD_AFTER_S, SnapshotDownload, SnapshotError,
    SnapshotRecord, bundle_head,
};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::s3;

fn unreadable(run_id: &str, why: &SnapshotError) -> Reply {
    let (resp, _) = error(
        422,
        "snapshot-unreadable",
        "the saved snapshot cannot be read: its files are not what agentd writes",
    );
    (resp, vec![format!("snapshot of run {run_id}: {why}")])
}

/// Seconds since the object was written, when it said so.
fn age_s(o: &ObjectPrefix) -> Option<i64> {
    let written = o.last_modified.as_deref().and_then(parse_utc_unix)?;
    Some(crate::input::unix_now() - written)
}

pub(crate) async fn download_url<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    };
    let none = || {
        error(
            404,
            "no-snapshot",
            "no snapshot has been saved for this run yet; agentd saves one about every 10 minutes while the files change",
        )
    };
    // A run that renewed its session keeps each session's snapshots under its own turn. The work in progress is the newest snapshot there
    // is: the run's current session first, then the ones before it.
    let mut found = None;
    for turn in (1..=run.slice.unwrap_or(1).max(1)).rev() {
        match deps
            .blobs
            .read_prefix(
                &s3::snapshot_json_key_t(run_id, turn),
                MAX_RECORD_BYTES as u64,
            )
            .await
        {
            Ok(Some(o)) => {
                found = Some((turn, o));
                break;
            }
            Ok(None) => {}
            Err(e) => return unavailable(&e, "retry"),
        }
    }
    let Some((turn, record_obj)) = found else {
        return none();
    };
    let bundle_key = s3::snapshot_bundle_key_t(run_id, turn);
    let bundle = match deps.blobs.read_prefix(&bundle_key, HEADER_BYTES).await {
        Ok(Some(o)) => o,
        Ok(None) => return none(),
        Err(e) => return unavailable(&e, "retry"),
    };
    // The record is read whole or not at all: a longer object would have been cut at the limit.
    if record_obj.size > MAX_RECORD_BYTES as u64 {
        return unreadable(
            run_id,
            &SnapshotError::Record(format!(
                "{} bytes; the limit is {MAX_RECORD_BYTES} bytes",
                record_obj.size
            )),
        );
    }
    let record = match SnapshotRecord::parse(&record_obj.bytes) {
        Ok(r) => r,
        Err(e) => return unreadable(run_id, &e),
    };
    let head = match bundle_head(&bundle.bytes) {
        Ok(h) => h,
        Err(e) => return unreadable(run_id, &SnapshotError::Bundle(e)),
    };
    let Some(etag) = bundle.etag.clone().filter(|t| !t.is_empty()) else {
        return unavailable(
            &crate::handler::StoreError::Unavailable("the bundle read returned no ETag".into()),
            "retry",
        );
    };
    let (taken_at, count, base_sha, record_stale) = if head == record.head_sha {
        (record.at, Some(record.count), record.base_sha, false)
    } else {
        // The record names another commit. A bundle written moments ago is a pair agentd is still saving (the bundle goes up first);
        // an older one is a record that will not catch up (its upload failed, or the session ended between the two).
        match age_s(&bundle) {
            Some(age) if age >= STALE_RECORD_AFTER_S => (
                bundle.last_modified.clone().unwrap_or_else(now_rfc3339),
                None,
                None,
                true,
            ),
            _ => {
                return error(
                    409,
                    "snapshot-updating",
                    "a new snapshot is being saved; ask again in a few seconds",
                );
            }
        }
    };
    // The version whose header was checked: an overwrite after this point (agentd saves a new bundle every ten minutes) cannot change
    // what the URL names. A store that keeps no versions sends none, and the client's If-Match on the ETag covers it.
    let signed = match deps
        .blobs
        .presign_get_version(&bundle_key, bundle.version_id.as_deref(), DOWNLOAD_URL_TTL)
        .await
    {
        Ok(u) => u,
        Err(e) => return unavailable(&e, "retry"),
    };
    json(
        200,
        &SnapshotDownload {
            url: signed.url,
            expires_at: signed.expires_at,
            head_sha: head,
            base_sha,
            taken_at,
            count,
            size_bytes: bundle.size,
            etag,
            record_stale,
            run_status: run.status,
        },
    )
}
