//! `POST /v1/runs/{runId}/snapshot/download-url`: a short-lived download URL for the latest saved copy of a run's work.
//!
//! agentd saves `snapshot.bundle` (a git bundle of `refs/heads/snapshot`) and then `snapshot.json` under
//! `runs/<run>/agents/direct/turns/<n>/` while the files change (every 15 seconds by default, and after a tool call) and once more when the engine has
//! stopped; the record of that last copy says `"final": true`. This route (owner only, whatever the run's status: a run that ended
//! blocked, cleared or with a zip answers like one that works) reads the record (at most 64 KiB) and the first 8 KiB of the bundle,
//! checks that both name the same commit, and presigns the bundle for fifteen minutes. The answer carries the bundle's size and ETag,
//! so the client can ask for the object only while it is still the one that was checked.
//!
//! Whether an object exists is asked by a listing (`Blobs::exists`), never by the read: the role may read these two keys, and S3 answers
//! the read of a key that does not exist with 403 for a role that may not list it, which would make "no copy" look like an outage.
//! `infra` grants the listing of exactly these keys (statement `ListSnapshot`).
//!
//! The worker role can write both objects, so the pair is worker-made: it is checked for consistency and shape, not for who wrote it
//! (`ham_core::snapshot::SNAPSHOT_TRUST_NOTE`). The role of this Lambda may get exactly these two keys (`infra`, statement
//! `SnapshotDownload`). The presigned URL is in the answer and nowhere else: it is in no diagnostic, and the access line holds the path only.
//!
//! Files the service read back from a stopped session's storage are one more object, written once by the worker of the read:
//! `runs/<run>/agents/recovery/turns/<n>/recovery.bundle`. The route serves them two ways.
//!
//! * **The copy the run goes on from.** When `next-slice` chose the files read back from session n, the Run item names them
//!   (`recoveredCopy`). They are the newest copy of the run until a later session saves one of its own, so the plain route answers
//!   with them while no session after n has a saved copy, and says so (`recovered`, `session`). A read-back object that is gone is
//!   no refusal: the route answers with the saved copy, as the delivery does.
//! * **The files read back from one session, used or not** (`?readBack=<n>`). The service uses files it read back only when its
//!   checks leave no doubt; files it did not use stay in the bucket, and this selector is how their owner gets them. It asks for the
//!   object of exactly that session and needs no word on the Run item.
//!
//! Either way the first bytes of the object must name a commit (and, for the chosen copy, the commit the service chose), and the
//! version that was checked is the one that is signed. `infra` grants the read and the listing of these keys beside the two above.

use crate::handler::{
    Blobs, DOWNLOAD_URL_TTL, Deps, Execution, ObjectPrefix, Reply, Store, V1Request, error, json,
    not_found, owned, unavailable,
};
use crate::ids::now_rfc3339;
use ham_core::snapshot::{
    HEADER_BYTES, MAX_RECORD_BYTES, STALE_RECORD_AFTER_S, SnapshotDownload, SnapshotError,
    SnapshotRecord, bundle_head,
};
use harness_protocol::common::{Engine, parse_utc_unix};
use harness_protocol::direct::{MAX_SLICES, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::RunItem;

/// What a run that works is told while it has no copy.
pub const NO_COPY_YET: &str =
    "no copy of this run's work is saved yet; ask again when the run has changed a file";
/// What a run that ended is told when it has no copy.
pub const NO_COPY: &str = "no copy of this run's work was saved";

/// The query member that asks for the files read back from one session: `?readBack=<n>`.
pub const READ_BACK_QUERY: &str = "readBack";
/// What a request is told whose `readBack` is no session number.
pub fn bad_read_back() -> String {
    format!("readBack names a session of the run by its number, 1 to {MAX_SLICES}")
}
/// What a request is told when no files were read back from the session it names.
pub fn no_read_back(session: u32) -> String {
    format!("no files were read back from the storage of session {session} of this run")
}

/// A read-back object that cannot be trusted: it is no bundle of the snapshot ref, or it is not the object the service chose.
/// Nothing is signed, and nothing is removed.
fn read_back_unreadable(run_id: &str, session: u32, why: &str) -> Reply {
    let (resp, _) = error(
        422,
        "snapshot-unreadable",
        "the files read back cannot be read: the object is not what agentd writes",
    );
    (
        resp,
        vec![format!(
            "read-back copy of run {run_id} session {session}: {why}"
        )],
    )
}

/// The answer for the read-back object of `session`: its header is read and must name a commit (`expect`: the commit and, when
/// the bucket keeps versions, the version the service chose), and the version that was read is the one that is signed. `None`: the
/// bucket holds no such object. `Some` is the reply, the download or the reason there is none.
async fn read_back<S: Store, B: Blobs, E: Execution>(
    run: &RunItem,
    session: u32,
    expect: Option<(&str, Option<&str>)>,
    taken_at: Option<&str>,
    deps: &Deps<S, B, E>,
) -> Option<Reply> {
    let run_id = run.run_id.as_str();
    let key = s3::recovery_bundle_key(run_id, session);
    match deps.blobs.exists(&key).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => return Some(unavailable(&e, "retry")),
    }
    let bundle = match deps.blobs.read_prefix(&key, HEADER_BYTES).await {
        Ok(Some(o)) => o,
        Ok(None) => return None,
        Err(e) => return Some(unavailable(&e, "retry")),
    };
    let head = match bundle_head(&bundle.bytes) {
        Ok(h) => h,
        Err(e) => return Some(read_back_unreadable(run_id, session, &e.to_string())),
    };
    if let Some((chosen_head, chosen_version)) = expect {
        if head != chosen_head {
            return Some(read_back_unreadable(
                run_id,
                session,
                "the object names another commit than the one the service chose",
            ));
        }
        if let (Some(chosen), Some(read)) = (chosen_version, bundle.version_id.as_deref())
            && chosen != read
        {
            return Some(read_back_unreadable(
                run_id,
                session,
                "the object is another version than the one the service chose",
            ));
        }
    }
    let Some(etag) = bundle.etag.clone().filter(|t| !t.is_empty()) else {
        return Some(unavailable(
            &crate::handler::StoreError::Unavailable("the bundle read returned no ETag".into()),
            "retry",
        ));
    };
    let signed = match deps
        .blobs
        .presign_get_version(&key, bundle.version_id.as_deref(), DOWNLOAD_URL_TTL)
        .await
    {
        Ok(u) => u,
        Err(e) => return Some(unavailable(&e, "retry")),
    };
    Some(json(
        200,
        &SnapshotDownload {
            url: signed.url,
            expires_at: signed.expires_at,
            head_sha: head,
            base_sha: None,
            // The time the service chose the copy, else the time the object was written.
            taken_at: taken_at
                .map(str::to_string)
                .or_else(|| bundle.last_modified.clone())
                .unwrap_or_else(now_rfc3339),
            count: None,
            size_bytes: bundle.size,
            etag,
            record_stale: false,
            run_status: run.status,
            final_copy: false,
            engine: Some(Engine::or_default(run.engine).as_str().to_string()),
            recovered: true,
            session: Some(session),
        },
    ))
}

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
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    // `?readBack=<n>` asks for the files read back from session n. The value is checked before anything is read.
    let asked = match req.query.get(READ_BACK_QUERY) {
        None => None,
        // A plain number of a session and nothing else: no sign, no blank, no leading zero.
        Some(text) => match text.parse::<u32>() {
            Ok(session) if (1..=MAX_SLICES).contains(&session) && session.to_string() == *text => {
                Some(session)
            }
            _ => return error(400, "bad-read-back", &bad_read_back()),
        },
    };
    let run = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    };
    // The files read back from one session, whether the service used them or not.
    if let Some(session) = asked {
        return match read_back(&run, session, None, None, deps).await {
            Some(reply) => reply,
            None => error(404, "no-read-back", &no_read_back(session)),
        };
    }
    // The fact, and nothing about why. A run that works is told when to ask again and is promised nothing; a run that ended has
    // no copy to come.
    let status = run.status;
    let none = || {
        error(
            404,
            "no-snapshot",
            if status == GoalStatus::Active {
                NO_COPY_YET
            } else {
                NO_COPY
            },
        )
    };
    // A run that renewed its session keeps each session's snapshots under its own turn. The work in progress is the newest snapshot there
    // is: the run's current session first, then the ones before it.
    // The bundle is the work and the record says what it is. A session that has a bundle has a copy, with or without its record: the
    // record goes up after the bundle, so a session that ended between the two, or whose record could not be stored, leaves the bundle
    // alone. A session that has no bundle (a record alone is left only by a deleted object) has no copy, and the session before it is
    // asked.
    let newest = run.slice.unwrap_or(1).max(1);
    let mut found = None;
    for turn in (1..=newest).rev() {
        match deps
            .blobs
            .exists(&s3::snapshot_bundle_key_t(run_id, turn))
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => return unavailable(&e, "retry"),
        }
        let key = s3::snapshot_json_key_t(run_id, turn);
        let record_obj = match deps.blobs.exists(&key).await {
            Ok(true) => match deps.blobs.read_prefix(&key, MAX_RECORD_BYTES as u64).await {
                Ok(o) => o,
                Err(e) => return unavailable(&e, "retry"),
            },
            Ok(false) => None,
            Err(e) => return unavailable(&e, "retry"),
        };
        found = Some((turn, record_obj));
        break;
    }
    // The copy the run goes on from is the files the service read back from session n when it chose them and no session after n
    // has saved a copy of its own. A later session's saved copy is newer and wins; files that were read back and not used are never
    // served here.
    if let Some(chosen) = run.recovered_copy.as_deref()
        && found
            .as_ref()
            .is_none_or(|(turn, _)| chosen.session >= *turn)
    {
        // An object that is gone is no refusal: the saved copy below is what there is.
        if let Some(reply) = read_back(
            &run,
            chosen.session,
            Some((chosen.head_sha.as_str(), chosen.version_id.as_deref())),
            Some(chosen.at.as_str()).filter(|t| parse_utc_unix(t).is_some()),
            deps,
        )
        .await
        {
            return reply;
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
    let record = match record_obj {
        // The record is read whole or not at all: a longer object would have been cut at the limit.
        Some(o) if o.size > MAX_RECORD_BYTES as u64 => {
            return unreadable(
                run_id,
                &SnapshotError::Record(format!(
                    "{} bytes; the limit is {MAX_RECORD_BYTES} bytes",
                    o.size
                )),
            );
        }
        Some(o) => match SnapshotRecord::parse(&o.bytes) {
            Ok(r) => Some(r),
            Err(e) => return unreadable(run_id, &e),
        },
        None => None,
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
    // The copy of the end of the run's current session is the work as the engine left it. The copy of the end of an earlier session is
    // not: the run worked on after it, and this is only the newest copy there is.
    // The record of this bundle: the one that names the bundle's commit.
    let record = record.filter(|r| r.head_sha == head);
    let final_copy = record.as_ref().is_some_and(|r| r.final_copy) && turn == newest;
    let (taken_at, count, base_sha, record_stale) = if let Some(record) = record {
        (record.at, Some(record.count), record.base_sha, false)
    } else {
        // There is no record, or it names another commit. A bundle a run that works wrote moments ago is a pair agentd is still saving
        // (the bundle goes up first). An older one, and any one of a run that ended, is a record that will not catch up (its upload
        // failed, or the session ended between the two).
        let settled = status != GoalStatus::Active
            || age_s(&bundle).is_some_and(|age| age >= STALE_RECORD_AFTER_S);
        if !settled {
            return error(
                409,
                "snapshot-updating",
                "a new snapshot is being saved; ask again in a few seconds",
            );
        }
        (
            bundle.last_modified.clone().unwrap_or_else(now_rfc3339),
            None,
            None,
            true,
        )
    };
    // The version whose header was checked: an overwrite after this point (agentd saves a new bundle while the run works) cannot change
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
            final_copy,
            engine: Some(Engine::or_default(run.engine).as_str().to_string()),
            recovered: false,
            session: None,
        },
    )
}
