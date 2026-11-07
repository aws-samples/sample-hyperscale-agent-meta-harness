//! `ham pull <runId> --snapshot` and `/pull --snapshot`: the last snapshot of a live run, downloaded to the download
//! directory with [`crate::chat_routes::fetch_snapshot`] (ranged download, size and commit check against the route's answer, rename after the
//! check). The command prints where the file is and what it holds; it checks nothing out and merges nothing. A snapshot is a worker-made git
//! bundle that the harness does not verify, and the text says so. No link is printed: the presigned URL never leaves `fetch_snapshot`.

use crate::api::Api;
use crate::chat_routes::{SnapshotFetched, fetch_snapshot};
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::sanitize::sanitize_line;
use ham_core::snapshot::{SNAPSHOT_REF, SNAPSHOT_TRUST_NOTE};
use serde_json::json;
use std::path::Path;

/// The exit code of a command that failed.
const ERROR: i32 = 1;

fn status_word(s: harness_protocol::goal::GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The branch name of [`SNAPSHOT_REF`], for the line that says how to look at the bundle.
fn branch() -> &'static str {
    SNAPSHOT_REF
        .strip_prefix("refs/heads/")
        .unwrap_or(SNAPSHOT_REF)
}

/// The text of a finished download: what and where, the commit, the size, the run's state and the trust note.
pub fn text(run_id: &str, f: &SnapshotFetched) -> String {
    let path = f.path.display().to_string();
    let which = match f.count {
        Some(n) => format!("snapshot {n}, taken {}", sanitize_line(&f.taken_at)),
        None => format!("taken {}", sanitize_line(&f.taken_at)),
    };
    let mut lines = vec![
        format!("pulled the snapshot of run {run_id}"),
        format!("File      {path}"),
        format!("Commit    {} ({which})", f.head_sha),
    ];
    if let Some(base) = &f.base_sha {
        lines.push(format!("Base      {base}"));
    }
    lines.push(format!("Size      {} bytes", f.size_bytes));
    lines.push(format!("Run       {}", status_word(f.run_status)));
    if f.record_stale {
        lines.push(
            "Note      the snapshot record lags the bundle: the commit above is the bundle's own"
                .into(),
        );
    }
    if f.run_status.is_terminal() {
        lines.push(format!(
            "Note      the run has ended: its delivered result is ham pull {run_id}, not this snapshot"
        ));
    }
    lines.push(format!("Look      git clone -b {} {path} <dir>", branch()));
    lines.push(format!("Warning   {SNAPSHOT_TRUST_NOTE}"));
    lines.join("\n")
}

/// The `--json` object: `runId` and `snapshot {path, headSha, baseSha?, takenAt, count?, sizeBytes, runStatus, recordStale}`.
pub fn json_of(run_id: &str, f: &SnapshotFetched) -> serde_json::Value {
    let mut s = serde_json::Map::new();
    s.insert("path".into(), json!(f.path.display().to_string()));
    s.insert("headSha".into(), json!(f.head_sha));
    if let Some(b) = &f.base_sha {
        s.insert("baseSha".into(), json!(b));
    }
    s.insert("takenAt".into(), json!(f.taken_at));
    if let Some(n) = f.count {
        s.insert("count".into(), json!(n));
    }
    s.insert("sizeBytes".into(), json!(f.size_bytes));
    s.insert("runStatus".into(), json!(status_word(f.run_status)));
    s.insert("recordStale".into(), json!(f.record_stale));
    s.insert("note".into(), json!(SNAPSHOT_TRUST_NOTE));
    json!({"runId": run_id, "snapshot": serde_json::Value::Object(s)})
}

/// Runs the command: 0 when the snapshot is on disk and checked, 1 with `ham: <reason>` on standard error otherwise.
pub async fn pull_snapshot<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    json: bool,
    dir: &Path,
    out: &Output,
) -> i32 {
    match fetch_snapshot(api, run_id, dir).await {
        Ok(f) => {
            if json {
                out.out_line(json_of(run_id, &f));
            } else {
                out.out_line(text(run_id, &f));
            }
            0
        }
        Err(e) => {
            out.err_line(format!("ham: {}", sanitize_line(&e.to_string())));
            ERROR
        }
    }
}
