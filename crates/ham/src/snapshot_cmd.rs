//! `ham pull <runId> --snapshot` and `/pull --snapshot`: the latest saved copy of a run's work, whether the run
//! works or has ended, downloaded to the download directory with [`crate::chat_routes::fetch_snapshot`] (ranged download, size and commit
//! check against the route's answer, rename after the check). The command prints where the file is and what it holds: the copy the
//! service saved when the engine had stopped is the work as the engine left it, and a copy made while the run worked says from when on
//! changes are missing. It checks nothing out and merges nothing. A snapshot is a worker-made git
//! bundle that the harness does not verify, and the text says so. No link is printed: the presigned URL never leaves `fetch_snapshot`.
//!
//! A copy can also be the files the service read back from the storage of a session after that session ended. `--snapshot`
//! downloads them when they are the copy the run goes on from, and says so. `ham pull <runId> --read-back <n>` downloads the files
//! read back from session `n` whether the run went on from them or not ([`crate::chat_routes::fetch_read_back`]); its text says what
//! the copy is and does not say that it is newer than the saved copy or that the run used it.

use crate::api::Api;
use crate::chat_routes::{SnapshotFetched, fetch_read_back, fetch_snapshot};
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::sanitize::sanitize_line;
use ham_core::snapshot::{SNAPSHOT_REF, SNAPSHOT_TRUST_NOTE};
use harness_protocol::goal::GoalStatus;
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

/// What the copy is. The copy of the end names the engine that ran; a copy made while the run worked says what it cannot hold.
/// `None` when the record lags the bundle (the note about the record says so, and what the copy is cannot be told).
fn saved_line(f: &SnapshotFetched) -> Option<String> {
    // Files read back are told in their own lines ([`read_back_note`]).
    if f.read_back_from.is_some() {
        return None;
    }
    if f.record_stale {
        None
    } else if f.final_copy {
        Some(ham_core::engines::say_for(
            f.engine.as_deref(),
            "Saved     the work as {engine} left it (saved when {engine} stopped)",
        ))
    } else {
        Some(format!(
            "Saved     while the run worked: changes after {} are not in it",
            sanitize_line(&f.taken_at)
        ))
    }
}

/// What a copy is that the service read back from the storage of session `n`. The run's newest copy (`--snapshot`) is such a
/// copy only when the service chose to go on from it. The copy of one named session (`--read-back <n>`) is what that storage held,
/// and nothing here says that it is newer than the saved copy or that the run went on from it.
pub fn read_back_note(n: u32, asked_read_back: bool) -> String {
    let what = format!(
        "This copy is what the service read back from the storage of session {n} after that session ended."
    );
    if asked_read_back {
        format!(
            "{what} The run may not have gone on from it: the service uses files it read back only when its checks leave no doubt."
        )
    } else {
        format!("{what} It is newer than the last copy saved while the run worked.")
    }
}

/// What the run's status means for the copy, in words that are true for that status; nothing for a run that works.
fn status_note(run_id: &str, status: GoalStatus) -> Option<String> {
    match status {
        GoalStatus::Active => None,
        GoalStatus::Complete | GoalStatus::Paused | GoalStatus::Stalled => Some(format!(
            "the run delivered a zip: ham pull {run_id} downloads it"
        )),
        GoalStatus::Blocked => {
            Some("the run is blocked and delivered no zip: this copy is its work".to_string())
        }
        GoalStatus::Cleared => {
            Some("the run was cleared and delivered nothing: this copy is its work".to_string())
        }
        GoalStatus::UsageLimited | GoalStatus::BudgetLimited => Some(
            "the run stopped at a limit and delivered no zip: this copy is its work".to_string(),
        ),
    }
}

/// The text of a finished download: what and where, the commit, the size, the run's state, what the copy is and the trust note.
pub fn text(run_id: &str, f: &SnapshotFetched) -> String {
    let path = f.path.display().to_string();
    let which = match (f.read_back_from, f.count) {
        (Some(n), _) => format!("read back from the storage of session {n}"),
        (None, Some(n)) => format!("snapshot {n}, taken {}", sanitize_line(&f.taken_at)),
        (None, None) => format!("taken {}", sanitize_line(&f.taken_at)),
    };
    let title = match f.read_back_from {
        Some(n) if f.asked_read_back => {
            format!("pulled the files read back from session {n} of run {run_id}")
        }
        _ => format!("pulled the snapshot of run {run_id}"),
    };
    let mut lines = vec![
        title,
        format!("File      {path}"),
        format!("Commit    {} ({which})", f.head_sha),
    ];
    if let Some(base) = &f.base_sha {
        lines.push(format!("Base      {base}"));
    }
    lines.push(format!("Size      {} bytes", f.size_bytes));
    lines.push(format!("Run       {}", status_word(f.run_status)));
    lines.extend(saved_line(f));
    if f.record_stale {
        lines.push(
            "Note      the snapshot record lags the bundle: the commit above is the bundle's own"
                .into(),
        );
    }
    if f.final_copy && f.base_sha.as_deref() == Some(f.head_sha.as_str()) {
        lines.push("Note      no file differs from the run's start".into());
    }
    if let Some(n) = f.read_back_from {
        lines.push(format!(
            "Note      {}",
            read_back_note(n, f.asked_read_back)
        ));
    }
    // The status note says "this copy is its work" of a run that delivered no zip. That is said of the run's newest copy, not of the
    // files read back from one named session, which the run may not have gone on from.
    if let Some(note) = status_note(run_id, f.run_status).filter(|_| !f.asked_read_back) {
        lines.push(format!("Note      {note}"));
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
    s.insert("finalCopy".into(), json!(f.final_copy));
    // Present only for files read back: the session whose storage they came from.
    if let Some(n) = f.read_back_from {
        s.insert("readBackSession".into(), json!(n));
    }
    s.insert("note".into(), json!(SNAPSHOT_TRUST_NOTE));
    json!({"runId": run_id, "snapshot": serde_json::Value::Object(s)})
}

/// Runs the command: 0 when the snapshot is on disk and checked, 1 with `ham: <reason>` on standard error otherwise. With
/// `read_back` it downloads the files read back from that session (`--read-back <n>`) in place of the run's newest copy.
pub async fn pull_snapshot<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    read_back: Option<u32>,
    json: bool,
    dir: &Path,
    out: &Output,
) -> i32 {
    let fetched = match read_back {
        Some(session) => fetch_read_back(api, run_id, session, dir).await,
        None => fetch_snapshot(api, run_id, dir).await,
    };
    match fetched {
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
