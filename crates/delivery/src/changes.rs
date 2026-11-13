//! The record of what a delivery changed: `runs/<run>/deliverable/changes.json`, written by the bundle validator beside the
//! verified bundle when it passes a delivery, from the same `base..head` changes it checked (`git diff-tree --no-renames`, so a rename is a
//! deletion and an addition). The `control-api` head route reads it (`ham_core::head::ChangeList::from_record`) so the follow-up digest can list the
//! files the run changed, as data. `crates/ham-core/tests/fixtures/changes/changes-v1.json` is the file both sides are tested against.
//!
//! The record names the run and the head it describes; a reader that finds another head's record does not use it. It lists at most
//! [`MAX_FILES`] paths of at most [`MAX_PATH_BYTES`] bytes, in path order, and says how many it left out (`total` counts every changed path). It is
//! an aid to the person's next prompt and never part of the delivery's pass or fail: the validator writes it after the checks and ignores a write
//! that fails.

use crate::git::Change;
use serde_json::json;

/// `schemaVersion` of `changes.json`.
pub const SCHEMA_VERSION: u32 = 1;
/// The most paths the record lists.
pub const MAX_FILES: usize = 2_000;
/// The most bytes of one listed path; a longer path is counted in `omitted`.
pub const MAX_PATH_BYTES: usize = 1024;
/// The most bytes of path text the record lists in all: the head route answers with the list inside the 6 MB of a Lambda response, whatever the
/// paths are made of. Paths past it are counted in `omitted`.
pub const MAX_LISTED_PATH_BYTES: usize = 512 * 1024;

/// `runs/<run>/deliverable/changes.json`.
pub fn changes_key(run_id: &str) -> String {
    format!("runs/{run_id}/deliverable/changes.json")
}

/// The record as JSON bytes. `base_sha` is the base the run started from, `None` for a run that started from an empty tree (every path is then an
/// addition). The listed paths are the first [`MAX_FILES`] in path order among those that fit; the others, and a path that holds a NUL or is longer
/// than [`MAX_PATH_BYTES`], are counted in `omitted`.
pub fn record(run_id: &str, head_sha: &str, base_sha: Option<&str>, changes: &[Change]) -> Vec<u8> {
    let mut listable: Vec<&Change> = changes
        .iter()
        .filter(|c| !c.path.is_empty() && c.path.len() <= MAX_PATH_BYTES && !c.path.contains('\0'))
        .collect();
    listable.sort_by(|a, b| a.path.cmp(&b.path));
    let mut used = 0usize;
    let files: Vec<serde_json::Value> = listable
        .iter()
        .take(MAX_FILES)
        .take_while(|c| {
            used += c.path.len();
            used <= MAX_LISTED_PATH_BYTES
        })
        .map(|c| json!({"path": c.path, "status": c.status.to_string()}))
        .collect();
    let total = changes.len();
    let omitted = total - files.len();
    let base = match base_sha {
        Some(sha) => json!({"kind": "commit", "sha": sha}),
        None => json!({"kind": "empty-tree"}),
    };
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "runId": run_id,
        "headSha": head_sha,
        "base": base,
        "total": total,
        "omitted": omitted,
        "files": files,
    })
    .to_string()
    .into_bytes()
}
