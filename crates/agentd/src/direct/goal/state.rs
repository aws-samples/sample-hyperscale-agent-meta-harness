//! The goal plugin's state file: `pendingGoal` before the engine claims the goal for its session, `activeGoals` (keyed by
//! session id) after, and the plugin removes the goal when the model completes it. agentd writes the pending goal before the first prompt
//! and clears the file to stop the loop; the plugin reads and writes the same file without a lock, so a read can meet half a write and agentd
//! never reads "no goal" from one read alone (the supervisor confirms it over time).
//!
//! The engine user owns the directory and can edit the file: what this module reads is a hint, never a done claim.

use crate::direct::snapshot::read_regular_nofollow;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

/// What one read of the state file showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalState {
    /// No such file, a link, a directory, or a file too big to be a state.
    Missing,
    /// The file is there and is not a state (empty, half written, not an object).
    Damaged,
    Seen {
        /// A goal waits for the engine to claim it.
        pending: bool,
        /// Goals claimed by a session and not completed.
        active: usize,
    },
}

/// The state that makes the engine claim `goal` at the start of its first run.
pub fn pending_state(goal: &str, created_at: &str) -> String {
    let mut text = json!({
        "version": 1,
        "pendingGoal": {"goal": goal, "createdAt": created_at},
        "activeGoals": {},
    })
    .to_string();
    text.push('\n');
    text
}

/// The state with no goal at all: the plugin queues no further prompt.
pub fn cleared_state() -> String {
    let mut text = json!({"version": 1, "activeGoals": {}}).to_string();
    text.push('\n');
    text
}

fn open_goal(v: &Value) -> bool {
    v.get("goal").is_some_and(Value::is_string) && v.get("completedAt").is_none_or(Value::is_null)
}

pub fn parse_state(bytes: &[u8]) -> GoalState {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return GoalState::Damaged;
    };
    let Some(obj) = v.as_object() else {
        return GoalState::Damaged;
    };
    let pending = obj.get("pendingGoal").is_some_and(open_goal);
    let active = obj
        .get("activeGoals")
        .and_then(Value::as_object)
        .map_or(0, |m| m.values().filter(|g| open_goal(g)).count());
    GoalState::Seen { pending, active }
}

pub fn read_state(path: &Path) -> GoalState {
    match read_regular_nofollow(path) {
        Some(bytes) => parse_state(&bytes),
        None => GoalState::Missing,
    }
}

/// Replaces the file whole: a temporary file in the same directory, then a rename (a link at `path` is replaced, never followed). The
/// file is 0644 and, when `owner` is given, belongs to the engine user so the plugin can rewrite it.
pub fn write_state(path: &Path, text: &str, owner: Option<(u32, u32)>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the state file has no directory"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{name}.agentd-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(&tmp, Some(uid), Some(gid))?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
