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
//!
//! A saved copy may leave out what the bundle the run started from holds (`runs/<run>/input/repo.bundle`, the base). The header
//! of such a copy names the commits it needs (`harness_protocol::bundle_header::needs`), and the route has read that header already.
//! A copy that names none is answered as before, with the reads it always made. For a copy that names one:
//!
//! * a request with `?parts=1` (a client that knows the member) gets the copy as before and a member `base`: the base object, read
//!   by one byte for its size, ETag and version, and signed for that version. Whether it is there is asked by a listing of its
//!   exact key first, like every key of this route. `infra` grants the read of that key (statement `SnapshotDownload`) and the
//!   listing of it (`ListSnapshot`). A base that is not there (no rule removes it; a hand did) is no refusal: the copy is intact
//!   and is the latest work, so it is served alone, with no member `base`, and the client keeps it and says what it needs.
//! * a request without it (a client that predates the member, which would download one file that does not clone alone) is told to
//!   update: 409 [`NEEDS_UPDATE_CODE`] with [`NEEDS_UPDATE`]. Nothing is signed, and the copy and the base stay in the bucket.
//!
//! The files read back from a stopped session are whole and are served as before either way.
//!
//! `?beforeRefusal=<notice id>` asks for one earlier copy of the run: the last copy a session had saved before its worker
//! first saw the session's storage refuse a write. The worker names that copy in its notice, with the version the bucket gave the
//! copy's bundle, and the notice's event id is the selector. The route reads that one event by its key, builds the key of the bundle
//! itself, reads the header at the version the notice names, and signs that version and no other ([`before_refusal`]). `infra`
//! grants nothing new: the role reads a named version of these keys already (statement `SnapshotDownload`).

use crate::handler::{
    Blobs, DOWNLOAD_URL_TTL, Deps, Execution, ObjectPrefix, Reply, Store, V1Request, error, json,
    not_found, owned, unavailable,
};
use crate::ids::now_rfc3339;
use ham_core::snapshot::{
    HEADER_BYTES, MAX_RECORD_BYTES, STALE_RECORD_AFTER_S, SnapshotBase, SnapshotDownload,
    SnapshotError, SnapshotRecord, bundle_head,
};
use harness_protocol::bundle_header::needs;
use harness_protocol::common::is_ulid;
use harness_protocol::common::{Engine, parse_utc_unix};
use harness_protocol::direct::{MAX_SLICES, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::RunItem;

/// What a run that works is told while it has no copy. The worker saves a copy when a file changed or when the
/// engine made a commit, so the sentence names both.
pub const NO_COPY_YET: &str = "no copy of this run's work is saved yet; ask again when the run has changed a file or made a commit";
/// What a run that ended is told when it has no copy.
pub const NO_COPY: &str = "no copy of this run's work was saved";

/// The query member of a client that takes a copy in two parts, the copy and the bundle the run started from: `?parts=1`.
/// Any other value, and no member, is a client that takes one file.
pub const PARTS_QUERY: &str = "parts";
/// The code of the answer for a copy that needs the run's base, asked for by a client that takes one file.
pub const NEEDS_UPDATE_CODE: &str = "snapshot-needs-update";
/// What such a client is told. The copy and the base stay in the bucket, and the same command works once `ham` is updated.
pub const NEEDS_UPDATE: &str = "this copy of the work needs a newer ham to download; the work is saved; run ham update and ask again";

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
            // The files read back are whole and are never served in parts.
            base: None,
            before_refusal: None,
        },
    ))
}

/// The query member that asks for the copy a notice of a storage refusal names: `?beforeRefusal=<notice id>`.
pub const BEFORE_REFUSAL_QUERY: &str = "beforeRefusal";
/// What a request is told whose `beforeRefusal` is no event id.
pub const BAD_BEFORE_REFUSAL: &str =
    "beforeRefusal names a notice of the run by its id, as the notice printed it";
/// What a request is told that sends `beforeRefusal` together with `readBack`.
pub const BEFORE_REFUSAL_ALONE: &str =
    "beforeRefusal stands alone, without readBack: each names one copy of the run";
/// What a request is told when the run has no notice of a storage refusal with the id it names.
pub const NO_BEFORE_REFUSAL: &str = "this run has no notice of a storage refusal with that id";
/// The longest version id S3 gives an object ("no more than 1,024 bytes long", How S3 Versioning works).
pub const MAX_VERSION_ID_BYTES: usize = 1024;

/// What a request is told when the notice it names had no copy to name.
pub fn no_copy_before_refusal(run_id: &str) -> String {
    format!("that notice names no copy; ham pull {run_id} --snapshot downloads the newest copy")
}

/// What a request is told when the store does not give the version the notice names: it is gone or cannot be read.
pub fn before_refusal_unavailable(run_id: &str) -> String {
    format!(
        "that copy cannot be read; a replaced copy is kept for {} days; ham pull {run_id} --snapshot downloads the newest copy",
        ham_core::storage_notice::REPLACED_COPY_DAYS
    )
}

/// The copy a notice of a storage refusal names, as the worker wrote it into the notice's `storage.copyBefore`.
struct CopyBefore {
    session: u32,
    count: u32,
    at: String,
    head_sha: String,
    version_id: String,
}

/// The `storage` member of `e` when `e` is a notice of a storage refusal of the direct agent of `run_id`; `None` for any
/// other event. The worker records such a notice with the run's completion key, which the engine does not hold.
fn storage_notice<'a>(
    e: &'a harness_protocol::envelope::Envelope,
    run_id: &str,
) -> Option<&'a serde_json::Value> {
    use harness_protocol::envelope::{Authority, EventType, Payload};
    if e.run_id != run_id
        || e.event_type != EventType::TurnProgress
        || e.authority != Authority::Agent
        || e.agent_id.as_deref() != Some(harness_protocol::direct::DIRECT_AGENT_ID)
    {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("notify")
        || value.get("reason").and_then(serde_json::Value::as_str) != Some("storage_refused")
    {
        return None;
    }
    value.get("storage").filter(|s| s.is_object())
}

/// The copy the notice's `storage` member names, when it names one in the worker's form: a session of the run, the copy's
/// number and time, a commit id and a version id of the bucket. Anything else is no copy.
fn copy_before(storage: &serde_json::Value) -> Option<CopyBefore> {
    let c = storage.get("copyBefore")?;
    let number = |name: &str| c.get(name)?.as_u64().and_then(|n| u32::try_from(n).ok());
    let text = |name: &str| c.get(name)?.as_str().map(str::to_string);
    let session = number("session").filter(|n| (1..=MAX_SLICES).contains(n))?;
    let count = number("count").filter(|n| *n >= 1)?;
    let at = text("at").filter(|t| parse_utc_unix(t).is_some())?;
    let head_sha = text("headSha").filter(|h| ham_core::snapshot::is_object_id(h))?;
    let version_id = text("versionId").filter(|v| {
        !v.is_empty() && v.len() <= MAX_VERSION_ID_BYTES && !v.chars().any(char::is_control)
    })?;
    Some(CopyBefore {
        session,
        count,
        at,
        head_sha,
        version_id,
    })
}

/// A version the notice names that is not what the notice says it is. Nothing is signed, and nothing else is served.
fn before_refusal_unreadable(run_id: &str, id: &str, why: &str) -> Reply {
    let (resp, _) = error(
        422,
        "snapshot-unreadable",
        "the saved snapshot cannot be read: its files are not what agentd writes",
    );
    (
        resp,
        vec![format!(
            "copy before the storage refusal {id} of run {run_id}: {why}"
        )],
    )
}

/// The bundle the run started from, for a copy that needs it. Whether it is there is asked by a listing of its exact key, as
/// for every key of this route: the role may not list the bucket at large, and S3 answers the read of an absent key with 403 for such
/// a role, which would make "no base" look like an outage for ever. Then one byte is read for its size, its ETag and its version. Both
/// come before anything is signed, so a store that does not answer signs nothing: `Err` is the reply that ends the request.
///
/// A base that is not there is no refusal (root invariant 9: a copy downloads however the run ended). No rule removes the base of a
/// run; a hand can. The copy is intact, so it is served alone, with no member `base` (`Ok(None)`, and `notes` says so): the client
/// keeps it and says that it needs the repository the run started from. The copy from before a storage refusal follows the
/// same rule, because the base does not change during a run.
async fn read_base<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    deps: &Deps<S, B, E>,
    notes: &mut Vec<String>,
) -> Result<Option<(String, ObjectPrefix, String)>, Reply> {
    let base_key = s3::repo_bundle_key(run_id);
    let object = match deps.blobs.exists(&base_key).await {
        Ok(true) => match deps.blobs.read_prefix(&base_key, 1).await {
            Ok(o) => o,
            Err(e) => return Err(unavailable(&e, "retry")),
        },
        Ok(false) => None,
        Err(e) => return Err(unavailable(&e, "retry")),
    };
    match object {
        Some(object) => {
            let Some(base_etag) = object.etag.clone().filter(|t| !t.is_empty()) else {
                return Err(unavailable(
                    &crate::handler::StoreError::Unavailable(
                        "the read of the bundle the run started from returned no ETag".into(),
                    ),
                    "retry",
                ));
            };
            Ok(Some((base_key, object, base_etag)))
        }
        None => {
            notes.push(format!(
                "snapshot of run {run_id}: the copy needs the bundle the run started from, and the bucket holds none; the copy is served alone"
            ));
            Ok(None)
        }
    }
}

/// The base that [`read_base`] read, signed for the version that was read.
async fn sign_base<S: Store, B: Blobs, E: Execution>(
    (base_key, object, base_etag): (String, ObjectPrefix, String),
    deps: &Deps<S, B, E>,
) -> Result<SnapshotBase, Reply> {
    match deps
        .blobs
        .presign_get_version(&base_key, object.version_id.as_deref(), DOWNLOAD_URL_TTL)
        .await
    {
        Ok(u) => Ok(SnapshotBase {
            url: u.url,
            size_bytes: object.size,
            etag: base_etag,
        }),
        Err(e) => Err(unavailable(&e, "retry")),
    }
}

/// The answer for `?beforeRefusal=<id>`: the copy that the notice `id` of the run names, the last copy the session had saved
/// before the worker first saw its storage refuse a write.
///
/// One event is read, by its exact key. It is taken only as a notice of a storage refusal of the direct agent that names a copy
/// ([`storage_notice`], [`copy_before`]). The key of the bundle is built here from the run and the session the notice names; nothing
/// of the event is used as a key. The header of the bundle is read at the version the notice names and must name the notice's commit,
/// and that version is the one that is signed: a newer object at the key changes nothing, and no other version is ever served in its
/// place. A version the store does not give has its own sentence.
///
/// The pointer is in an event and not in the bucket: events are written with a key the engine does not hold, and the engine can
/// neither change nor delete a stored version, so the answer rests on nothing the engine can write.
///
/// A copy that needs the bundle the run started from is answered as the newest copy is: in two parts for a request that
/// said `parts=1`, and with 409 [`NEEDS_UPDATE_CODE`] for one that did not. The base does not change during a run, so the base of
/// today is the base of that copy.
async fn before_refusal<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run: &RunItem,
    id: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run_id = run.run_id.as_str();
    let event = match deps
        .store
        .get_event(run_id, crate::input::direct_shard(), id)
        .await
    {
        Ok(e) => e,
        Err(e) => return unavailable(&e, "retry"),
    };
    let Some(storage) = event.as_ref().and_then(|e| storage_notice(e, run_id)) else {
        return error(404, "no-before-refusal", NO_BEFORE_REFUSAL);
    };
    let Some(copy) = copy_before(storage) else {
        return error(
            404,
            "no-copy-before-refusal",
            &no_copy_before_refusal(run_id),
        );
    };
    let key = s3::snapshot_bundle_key_t(run_id, copy.session);
    let bundle = match deps
        .blobs
        .read_prefix_version(&key, &copy.version_id, HEADER_BYTES)
        .await
    {
        Ok(Some(o)) => o,
        Ok(None) => {
            return error(
                404,
                "before-refusal-unavailable",
                &before_refusal_unavailable(run_id),
            );
        }
        Err(e) => return unavailable(&e, "retry"),
    };
    // The store answered for the version that was asked for, and its bytes are a bundle of the commit the notice names.
    if bundle
        .version_id
        .as_deref()
        .is_some_and(|read| read != copy.version_id)
    {
        return before_refusal_unreadable(
            run_id,
            id,
            "the store answered with another version than the one the notice names",
        );
    }
    let head = match bundle_head(&bundle.bytes) {
        Ok(h) => h,
        Err(e) => return before_refusal_unreadable(run_id, id, &e.to_string()),
    };
    if head != copy.head_sha {
        return before_refusal_unreadable(
            run_id,
            id,
            "the version names another commit than the notice",
        );
    }
    let needs_base = match needs(&bundle.bytes) {
        Ok(commits) => !commits.is_empty(),
        Err(e) => return before_refusal_unreadable(run_id, id, &e.to_string()),
    };
    let parts = req.query.get(PARTS_QUERY).is_some_and(|v| v == "1");
    if needs_base && !parts {
        return error(409, NEEDS_UPDATE_CODE, NEEDS_UPDATE);
    }
    let Some(etag) = bundle.etag.clone().filter(|t| !t.is_empty()) else {
        return unavailable(
            &crate::handler::StoreError::Unavailable("the bundle read returned no ETag".into()),
            "retry",
        );
    };
    let mut notes = Vec::new();
    let base_object = if needs_base {
        match read_base(run_id, deps, &mut notes).await {
            Ok(base) => base,
            Err(reply) => return reply,
        }
    } else {
        None
    };
    let signed = match deps
        .blobs
        .presign_get_version(&key, Some(&copy.version_id), DOWNLOAD_URL_TTL)
        .await
    {
        Ok(u) => u,
        Err(e) => return unavailable(&e, "retry"),
    };
    let base = match base_object {
        Some(read) => match sign_base(read, deps).await {
            Ok(base) => Some(base),
            Err(reply) => return reply,
        },
        None => None,
    };
    let (resp, _) = json(
        200,
        &SnapshotDownload {
            url: signed.url,
            expires_at: signed.expires_at,
            head_sha: head,
            base_sha: None,
            // The time the copy names: from before its tree was read, as the notice and the copy's own record say it.
            taken_at: copy.at,
            count: Some(copy.count),
            size_bytes: bundle.size,
            etag,
            record_stale: false,
            run_status: run.status,
            // The copy was made while the run worked: a refusal was seen after it.
            final_copy: false,
            engine: Some(Engine::or_default(run.engine).as_str().to_string()),
            recovered: false,
            session: None,
            base,
            before_refusal: Some(id.to_string()),
        },
    );
    (resp, notes)
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
    // `?beforeRefusal=<notice id>` asks for the copy a notice of a storage refusal names. The value must be an event id (26
    // characters of the id alphabet) and stand alone, before anything is read.
    let before = match req.query.get(BEFORE_REFUSAL_QUERY) {
        None => None,
        Some(id) if !is_ulid(id) => return error(400, "bad-before-refusal", BAD_BEFORE_REFUSAL),
        Some(_) if asked.is_some() => {
            return error(400, "bad-before-refusal", BEFORE_REFUSAL_ALONE);
        }
        Some(id) => Some(id.as_str()),
    };
    let run = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    };
    // The copy the notice names, and nothing else in its place.
    if let Some(id) = before {
        return before_refusal(req, &run, id, deps).await;
    }
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
    // What the copy needs and does not hold, from the header bytes read above. A copy that names no commit is answered as it
    // always was. A copy that names one needs the bundle the run started from: a client that takes one file is told to update, before
    // anything else is read or signed.
    let needs_base = match needs(&bundle.bytes) {
        Ok(commits) => !commits.is_empty(),
        Err(e) => return unreadable(run_id, &SnapshotError::Bundle(e)),
    };
    let parts = req.query.get(PARTS_QUERY).is_some_and(|v| v == "1");
    if needs_base && !parts {
        return error(409, NEEDS_UPDATE_CODE, NEEDS_UPDATE);
    }
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
    // The bundle the run started from, for a copy that needs it. Whether it is there is asked by a listing of its exact
    // key, as for every key of this route: the role may not list the bucket at large, and S3 answers the read of an absent key with
    // 403 for such a role, which would make "no base" look like an outage for ever. Then one byte is read for its size, its ETag and
    // its version. Both come before anything is signed, so a store that does not answer signs nothing.
    // A base that is not there is no refusal (root invariant 9: the latest copy downloads however the run ended). No rule removes
    // the base of a run; a hand can. The copy is intact, so it is served alone, with no member `base`: the client keeps it and says
    // that it needs the repository the run started from.
    let mut notes = Vec::new();
    let base_object = if needs_base {
        match read_base(run_id, deps, &mut notes).await {
            Ok(base) => base,
            Err(reply) => return reply,
        }
    } else {
        None
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
    let base = match base_object {
        Some(read) => match sign_base(read, deps).await {
            Ok(base) => Some(base),
            Err(reply) => return reply,
        },
        None => None,
    };
    let (resp, _) = json(
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
            base,
            before_refusal: None,
        },
    );
    (resp, notes)
}
