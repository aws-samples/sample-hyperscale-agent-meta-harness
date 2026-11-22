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
//!
//! `ham pull <runId> --before-refusal <notice id>` downloads the last copy a session had saved before `ham` first saw the
//! session's storage refuse a write ([`crate::chat_routes::fetch_before_refusal`]); the id is the one the run's notice printed. Its
//! text says what the copy is and what it cannot promise: changes after its time are not in it, and a refusal that `ham` did not see
//! can have emptied files in it too. A copy that needs the repository the run started from comes with it, as the newest copy does.

use crate::api::Api;
use crate::chat_routes::{SnapshotFetched, fetch_before_refusal, fetch_read_back, fetch_snapshot};
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::sanitize::sanitize_line;
use ham_core::snapshot::{SNAPSHOT_REF, SNAPSHOT_TRUST_NOTE};
use harness_protocol::goal::GoalStatus;
use serde_json::json;
use std::path::{Path, PathBuf};

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
    // The copy from before a storage refusal says what it is, with its number and its time to the minute.
    if f.before_refusal.is_some() {
        let when = utc_minute(&sanitize_line(&f.taken_at));
        return Some(match f.count {
            Some(n) => format!("Saved     copy {n}, {when}: {BEFORE_REFUSAL_SAVED}"),
            None => format!("Saved     {when}: {BEFORE_REFUSAL_SAVED}"),
        });
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

/// What the copy from before a storage refusal is.
pub const BEFORE_REFUSAL_SAVED: &str =
    "the last copy saved before ham first saw the session's storage refuse a write";
/// What that copy cannot promise.
pub const BEFORE_REFUSAL_NOTE: &str = "Changes after that time are not in it. A refusal that ham did not see can have emptied files in it too.";

/// `2026-11-21 00:37 UTC` for `2026-11-21T00:37:12Z`: a time to the minute. A text that is no such time is returned as it is.
fn utc_minute(at: &str) -> String {
    let b = at.as_bytes();
    if b.len() >= 20 && b[10] == b'T' && at.ends_with('Z') && at.is_char_boundary(16) {
        format!("{} {} UTC", &at[..10], &at[11..16])
    } else {
        at.to_string()
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
        _ if f.before_refusal.is_some() => {
            format!("pulled the copy from before a storage refusal of run {run_id}")
        }
        _ => format!("pulled the snapshot of run {run_id}"),
    };
    let mut lines = vec![title, format!("File      {path}")];
    // A copy that holds what the run changed comes with the repository the run started from, as a second file.
    if let Some((base_path, _)) = &f.base {
        lines.push(format!(
            "Needs     {} (the repository the run started from)",
            base_path.display()
        ));
    }
    lines.push(format!("Commit    {} ({which})", f.head_sha));
    if let Some(base) = &f.base_sha {
        lines.push(format!("Base      {base}"));
    }
    lines.push(match &f.base {
        Some((_, base_bytes)) => format!(
            "Size      {} bytes, and {base_bytes} bytes for the repository the run started from",
            f.size_bytes
        ),
        None => format!("Size      {} bytes", f.size_bytes),
    });
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
    if f.before_refusal.is_some() {
        lines.push(format!("Note      {BEFORE_REFUSAL_NOTE}"));
    }
    // The status note says "this copy is its work" of a run that delivered no zip. That is said of the run's newest copy, not of the
    // files read back from one named session, which the run may not have gone on from, and not of a copy from before a storage
    // refusal, which the run worked on after.
    if let Some(note) = status_note(run_id, f.run_status)
        .filter(|_| !f.asked_read_back && f.before_refusal.is_none())
    {
        lines.push(format!("Note      {note}"));
    }
    lines.push(look_line(f));
    lines.push(format!("Warning   {SNAPSHOT_TRUST_NOTE}"));
    lines.join("\n")
}

/// What a copy that needs the repository the run started from is told when the stage sent no such file with it. The page it
/// names is the user guide's, which every reader of the product has; it holds the commands that put the two files together.
pub const NEEDS_BASE_NOTE: &str = "this copy holds what the run changed and needs the repository the run started from, which this stage did not send; see \"Pull the work in progress\" in the user guide";

/// The local branch the look line of a copy in two files puts the copy on. It is not the copy's own name (`snapshot`): a
/// repository whose own branch has that name is on it after the clone, and git refuses to fetch into the branch that is checked out.
const LOOK_BRANCH: &str = "ham-snapshot";

/// A path as one word of a command line a person pastes into a shell: as it is when it holds only characters a shell reads as
/// themselves, else in single quotes, with a single quote inside written as `'\''`.
fn shell_word(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The whole path of a file of the download folder, for a command that changes folder before it reads the file. A download folder
/// that was given as a relative path (`HAMH_DOWNLOAD_DIR=out`, `downloadDir` in `config.toml`) is counted from the folder `ham`
/// runs in; no file is read for it and no link is followed. A path that cannot be made whole stays as it is.
fn whole_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The line that says how to look at the copy: one clone for a copy that holds the whole history; for a copy that came with the
/// repository the run started from, the three git commands that put the two files together; and for a copy that needs that
/// repository and came without it, a note in place of a command that would fail.
///
/// The three commands fetch the copy without naming a local branch and then put it on [`LOOK_BRANCH`], so they work whatever the
/// branches of the repository the run started from are called. Each path is one word of the line ([`shell_word`]). The second
/// command changes to `<dir>` before it fetches (`git -C <dir>`), so the three commands name both files by their whole paths
/// ([`whole_path`]): with a download folder given as a relative path the line still works where it is pasted.
fn look_line(f: &SnapshotFetched) -> String {
    let path = shell_word(&f.path);
    match &f.base {
        Some((base_path, _)) => {
            let path = shell_word(&whole_path(&f.path));
            let base_path = shell_word(&whole_path(base_path));
            let branch = branch();
            format!(
                "Look      git clone {base_path} <dir> && git -C <dir> fetch {path} {branch} && git -C <dir> checkout -B {LOOK_BRANCH} FETCH_HEAD"
            )
        }
        None if f.needs_base => format!("Note      {NEEDS_BASE_NOTE}"),
        None => format!("Look      git clone -b {} {path} <dir>", branch()),
    }
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
    // Present only for a copy that needs the repository the run started from: the second file, or that the stage sent none.
    if let Some((base_path, base_bytes)) = &f.base {
        s.insert(
            "base".into(),
            json!({"path": base_path.display().to_string(), "sizeBytes": base_bytes}),
        );
    }
    if f.needs_base {
        s.insert("needsBase".into(), json!(true));
    }
    // Present only for the copy from before a storage refusal: the id of the notice that names it.
    if let Some(id) = &f.before_refusal {
        s.insert("beforeRefusal".into(), json!(id));
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
    say(run_id, fetched, json, out)
}

/// `ham pull <runId> --before-refusal <notice id>`: 0 when the copy the notice names is on disk and checked, 1 with
/// `ham: <reason>` on standard error otherwise.
pub async fn pull_before_refusal<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    notice_id: &str,
    json: bool,
    dir: &Path,
    out: &Output,
) -> i32 {
    say(
        run_id,
        fetch_before_refusal(api, run_id, notice_id, dir).await,
        json,
        out,
    )
}

/// Prints what a download came to and returns the exit code.
fn say(
    run_id: &str,
    fetched: Result<SnapshotFetched, crate::chat_routes::SnapshotFetchError>,
    json: bool,
    out: &Output,
) -> i32 {
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
