//! The latest saved copy of a run's work: `POST /v1/runs/{runId}/snapshot/download-url`.
//!
//! agentd saves the work tree while the files change (every 15 seconds by default, and after a tool call) and once more when the engine has stopped:
//! `snapshot.bundle` (a git bundle of `refs/heads/snapshot`), then `snapshot.json` beside it. The copy of the end carries `"final": true`
//! and is the session's final head, so the route answers for a run that ended as it does for one that works. The route reads the record and the first bytes of the bundle, checks that both name the same commit,
//! and presigns the bundle. The worker role can write both objects, so the pair is **worker-made and not verified by the harness**
//! ([`SNAPSHOT_TRUST_NOTE`]): matching commits show that two files agree, not who wrote them.
//!
//! The route also serves the files the service read back from a stopped session's storage (one bundle of the same ref, written
//! once by the worker of the read): as the run's newest copy when the service chose them and no later session has saved one, and for
//! one named session with `?readBack=<n>`, used or not. The answer then says `recovered` and names the `session`.
//!
//! A saved copy may hold what the run changed and leave out what the bundle the run started from holds; its header then names
//! the commits it needs. The route answers such a copy with a second object, [`SnapshotBase`] (the member `base`), for a client that
//! asked for the copy in parts, and tells a client that takes one file to update.
//!
//! `control-api` runs [`bundle_head`] on the stored bundle's first [`HEADER_BYTES`] bytes; `ham` runs [`verify_download`] on what it
//! downloaded. A presigned URL is a bearer capability: [`SnapshotDownload`] hides it from `Debug`, and nothing logs it.

use harness_protocol::common::is_utc_timestamp;
use harness_protocol::goal::GoalStatus;
use serde::{Deserialize, Serialize};

// The header functions live in `harness_protocol::bundle_header`, so that the worker and the delivery read a header with the
// same code as the service and the client; they are re-exported here, where the service and the client have always imported them.
pub use harness_protocol::bundle_header::{
    BundleError, HEADER_BYTES, SNAPSHOT_REF, bundle_head, bundle_head_of, is_object_id,
};

/// The most bytes of `snapshot.json` read; agentd writes about 150.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
/// A bundle written less than this long ago that disagrees with its record is a pair being saved; an older one is a record that lags.
pub const STALE_RECORD_AFTER_S: i64 = 120;
/// What a client says beside the files it downloads.
pub const SNAPSHOT_TRUST_NOTE: &str =
    "A worker snapshot, not checked by the harness: treat its contents as untrusted code and data.";

/// Why a snapshot is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot record: {0}")]
    Record(String),
    #[error("snapshot bundle: {0}")]
    Bundle(#[from] BundleError),
    #[error("the bundle is {actual} bytes; the route said {expected}")]
    Size { expected: u64, actual: u64 },
    #[error("the bundle holds commit {actual}; the route said {expected}")]
    Head { expected: String, actual: String },
}

/// `snapshot.json` as agentd writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub at: String,
    pub count: u32,
    /// The copy was saved when the engine had stopped: it is the end state of the session's work. `false` for a copy made
    /// while the run worked and for every record written before the member existed.
    pub final_copy: bool,
}

impl SnapshotRecord {
    /// Parses and validates the record. The worker can write it, so every member is checked and none is echoed unchecked.
    pub fn parse(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let bad = |m: String| SnapshotError::Record(m);
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(bad(format!(
                "{} bytes; the limit is {MAX_RECORD_BYTES} bytes",
                bytes.len()
            )));
        }
        let v: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| bad(format!("not JSON: {e}")))?;
        if v.get("schemaVersion").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(bad("schemaVersion is not 1".into()));
        }
        let text = |name: &str| v.get(name).and_then(serde_json::Value::as_str);
        let head_sha = text("headSha")
            .filter(|s| is_object_id(s))
            .ok_or_else(|| bad("headSha is not a full object id".into()))?
            .to_string();
        let base_sha = match v.get("baseSha") {
            None | Some(serde_json::Value::Null) => None,
            Some(b) => Some(
                b.as_str()
                    .filter(|s| is_object_id(s))
                    .ok_or_else(|| bad("baseSha is not a full object id".into()))?
                    .to_string(),
            ),
        };
        let at = text("at")
            .filter(|s| is_utc_timestamp(s))
            .ok_or_else(|| bad("at is not a UTC time".into()))?
            .to_string();
        let count = v
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad("count is not a whole number".into()))?;
        // Anything but the literal `true` is a copy made while the run worked.
        let final_copy = v.get("final").and_then(serde_json::Value::as_bool) == Some(true);
        Ok(Self {
            head_sha,
            base_sha,
            at,
            count,
            final_copy,
        })
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The answer of the download route. `url` is a bearer capability and `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDownload {
    pub url: String,
    /// The URL's true expiry (the sooner of fifteen minutes and the signing credentials' own).
    pub expires_at: String,
    /// The commit of the bundle's [`SNAPSHOT_REF`], read from the bundle itself.
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// When the snapshot was taken (the record's time), or the bundle's write time when the record lags.
    pub taken_at: String,
    /// The number of the snapshot in the run; absent when the record lags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    pub size_bytes: u64,
    /// The ETag of the bundle whose header was checked: send it as `If-Match` on every ranged GET (the URL itself names that version).
    pub etag: String,
    /// The record named another commit than the bundle and the bundle is older than [`STALE_RECORD_AFTER_S`]: the commit above is the
    /// bundle's own and `count` and `base_sha` are not known.
    #[serde(default, skip_serializing_if = "is_false")]
    pub record_stale: bool,
    /// The run's status when asked: a finished run's last snapshot is not its delivered result (`ham pull` is).
    pub run_status: GoalStatus,
    /// The copy was saved when the engine had stopped (the record's `final`): it holds the work as the engine left it. Absent
    /// for a copy made while the run worked, when the record lags, and from a service older than the member.
    #[serde(default, skip_serializing_if = "is_false")]
    pub final_copy: bool,
    /// The engine of the run (its word, as the service stores it), so a sentence about the copy names the engine that ran. Absent
    /// from a service older than the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The copy is what the service read back from the storage of a session after that session ended, not a copy the session
    /// saved while it worked. Absent for a saved copy and from a service older than the member.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recovered: bool,
    /// The session whose storage the copy was read back from; present with `recovered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
    /// The copy holds what the run changed and needs the bundle the run started from, which is this object: download both,
    /// the base first for whoever puts them together. Present only for such a copy and only when the request said `parts=1`; absent
    /// for a copy that clones alone and from a service older than the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<SnapshotBase>,
    /// The answer is the copy that the notice of a storage refusal with this event id names (`?beforeRefusal=<id>`): the
    /// last copy the session had saved before its worker first saw the storage refuse a write, at the version the notice names.
    /// Present only in the answer to such a request; a service older than the member ignores the query and answers with the run's
    /// newest copy and no such member, which is how a client tells the two apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_refusal: Option<String>,
}

/// The bundle a run started from, as the download route names it beside a copy that needs it. `url` is a bearer capability and
/// `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBase {
    pub url: String,
    pub size_bytes: u64,
    /// The ETag of the object that was read: send it as `If-Match` on every ranged GET.
    pub etag: String,
}

impl std::fmt::Debug for SnapshotBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotBase")
            .field("url", &"<hidden>")
            .field("size_bytes", &self.size_bytes)
            .field("etag", &self.etag)
            .finish()
    }
}

impl std::fmt::Debug for SnapshotDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotDownload")
            .field("url", &"<hidden>")
            .field("expires_at", &self.expires_at)
            .field("head_sha", &self.head_sha)
            .field("base_sha", &self.base_sha)
            .field("taken_at", &self.taken_at)
            .field("count", &self.count)
            .field("size_bytes", &self.size_bytes)
            .field("etag", &self.etag)
            .field("record_stale", &self.record_stale)
            .field("run_status", &self.run_status)
            .field("final_copy", &self.final_copy)
            .field("engine", &self.engine)
            .field("recovered", &self.recovered)
            .field("session", &self.session)
            .field("base", &self.base)
            .field("before_refusal", &self.before_refusal)
            .finish()
    }
}

/// Checks a downloaded object against the route's answer: `prefix` holds at least its header, `size` is its whole length.
pub fn verify_object(prefix: &[u8], size: u64, d: &SnapshotDownload) -> Result<(), SnapshotError> {
    if size != d.size_bytes {
        return Err(SnapshotError::Size {
            expected: d.size_bytes,
            actual: size,
        });
    }
    let head = bundle_head(prefix)?;
    if head != d.head_sha {
        return Err(SnapshotError::Head {
            expected: d.head_sha.clone(),
            actual: head,
        });
    }
    Ok(())
}

/// [`verify_object`] for the whole object in memory.
pub fn verify_download(bytes: &[u8], d: &SnapshotDownload) -> Result<(), SnapshotError> {
    verify_object(bytes, bytes.len() as u64, d)
}
