//! The work in progress: `POST /v1/runs/{runId}/snapshot/download-url`.
//!
//! agentd saves the work tree about every 10 minutes while the files change: `snapshot.bundle` (a git bundle of `refs/heads/snapshot`),
//! then `snapshot.json` beside it. The route reads the record and the first bytes of the bundle, checks that both name the same commit,
//! and presigns the bundle. The worker role can write both objects, so the pair is **worker-made and not verified by the harness**
//! ([`SNAPSHOT_TRUST_NOTE`]): matching commits show that two files agree, not who wrote them.
//!
//! `control-api` runs [`bundle_head`] on the stored bundle's first [`HEADER_BYTES`] bytes; `ham` runs [`verify_download`] on what it
//! downloaded. A presigned URL is a bearer capability: [`SnapshotDownload`] hides it from `Debug`, and nothing logs it.

use harness_protocol::common::is_utc_timestamp;
use harness_protocol::goal::GoalStatus;
use serde::{Deserialize, Serialize};

/// The ref agentd bundles.
pub const SNAPSHOT_REF: &str = "refs/heads/snapshot";
/// The bytes of the bundle read to find its header; a header that does not end inside them is refused.
pub const HEADER_BYTES: u64 = 8192;
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

/// Why a bundle header cannot be trusted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    #[error("not a git bundle (v2 or v3)")]
    NotABundle,
    #[error("the header does not end within the bytes read")]
    Truncated,
    #[error("the bundle has no {SNAPSHOT_REF}")]
    NoSnapshotRef,
    /// [`bundle_head_of`]: no line names the ref that was asked for.
    #[error("the bundle does not name the ref asked for")]
    MissingRef,
    #[error("the bundle names {SNAPSHOT_REF} more than once")]
    Ambiguous,
    /// [`bundle_head_of`]: more than one line names the ref that was asked for (named here, so the words are about the right ref).
    #[error("the bundle names {0} more than once")]
    AmbiguousRef(String),
    /// A header line that does not start with a full object id. It holds the length of the offending text only: the bytes are the
    /// worker's, and an error is logged.
    #[error("a header line does not start with a full object id")]
    BadObjectId(usize),
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits. An abbreviation cannot be compared with a header's ids.
pub fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `snapshot.json` as agentd writes it (`{"schemaVersion":1,"headSha","baseSha","at","count"}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub at: String,
    pub count: u32,
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
        Ok(Self {
            head_sha,
            base_sha,
            at,
            count,
        })
    }
}

/// The commit a bundle's header names for [`SNAPSHOT_REF`]. `prefix` is the first bytes of the bundle (at least up to the blank line that
/// ends the header; the pack may be cut anywhere). A v3 header's `@capability` lines and the `-<id> subject` prerequisite lines are
/// skipped; exactly one `<id> refs/heads/snapshot` line must remain.
pub fn bundle_head(prefix: &[u8]) -> Result<String, BundleError> {
    bundle_head_of(prefix, SNAPSHOT_REF).map_err(|e| match e {
        BundleError::MissingRef => BundleError::NoSnapshotRef,
        BundleError::AmbiguousRef(_) => BundleError::Ambiguous,
        other => other,
    })
}

/// The commit a bundle's header names for `wanted` (a full ref name such as `refs/heads/harness/<runId>`); everything else is as for
/// [`bundle_head`]: the prefix must hold the whole header, and exactly one line must name the ref.
pub fn bundle_head_of(prefix: &[u8], wanted: &str) -> Result<String, BundleError> {
    let mut lines: Vec<&[u8]> = prefix.split(|b| *b == b'\n').collect();
    // The last piece has no newline after it: the empty rest, or a line the prefix cut in two. Neither is read.
    lines.pop();
    let mut it = lines.into_iter();
    match it.next() {
        Some(b"# v2 git bundle") | Some(b"# v3 git bundle") => {}
        None if prefix.starts_with(b"# v") => return Err(BundleError::Truncated),
        _ => return Err(BundleError::NotABundle),
    }
    let mut found: Option<String> = None;
    let mut ended = false;
    for line in it {
        if line.is_empty() {
            ended = true;
            break;
        }
        if line[0] == b'@' || line[0] == b'-' {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        let Some((oid, name)) = text.split_once(' ') else {
            return Err(BundleError::BadObjectId(text.len()));
        };
        if !is_object_id(oid) {
            return Err(BundleError::BadObjectId(oid.len()));
        }
        if name == wanted {
            if found.is_some() {
                return Err(BundleError::AmbiguousRef(wanted.to_string()));
            }
            found = Some(oid.to_string());
        }
    }
    if !ended {
        return Err(BundleError::Truncated);
    }
    found.ok_or(BundleError::MissingRef)
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
