//! Placing the user's attachments: `<agent data>/context/attachments/`.
//!
//! ```text
//! context/attachments/              0755, made by agentd (root in a microVM); the engine reads it and cannot write in it
//!   .agentd-placed                  0444, the marker: agentd made this directory, and a later attempt of the same run may replace it
//!   <name>                          0444, the file as the user sent it, byte for byte
//!   INDEX.json  INDEX.md            0444, one row per file: name, SHA-256, bytes, path
//! ```
//!
//! Rules: a link or a file where the directory belongs is refused and never followed; a directory that is there without the marker is not
//! agentd's, so it is never written into or removed ("already there"); a directory with the marker is what an earlier attempt of this run
//! placed, so it is replaced (a second invocation of the same run on the same VM starts from what the first one found). Every file is created
//! new and never through a link. A name that is not a plain file name is refused here too, whatever the caller checked. A failed placement
//! removes only what it made.

use crate::direct::context::ContextDir;
use harness_protocol::direct::{Attachment, is_reserved_attachment_name, valid_attachment_name};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The directory under the context directory.
pub const DIR: &str = "attachments";
/// The file that says agentd made the directory.
pub const MARKER: &str = ".agentd-placed";
pub const INDEX_JSON: &str = "INDEX.json";
pub const INDEX_MD: &str = "INDEX.md";
/// `schema` of `INDEX.json`.
pub const INDEX_SCHEMA: &str = "hamh-attachments-index/1";

/// One placed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedItem {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub path: PathBuf,
}

/// What was placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub dir: PathBuf,
    pub items: Vec<PlacedItem>,
}

fn io(what: &str, e: &std::io::Error) -> String {
    format!("attachments: {what}: {}", e.kind())
}

/// Writes a new file with mode 0444 and the given bytes; a link or an existing file of that name is an error.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| io("a file", &e))?;
    file.write_all(bytes).map_err(|e| io("a file", &e))
}

/// Removes the files of a directory agentd made (regular files only; anything else is a reason to stop), then the directory.
fn retire(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| io("the directory", &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io("the directory", &e))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| io("the directory", &e))?;
        if !kind.is_file() {
            return Err(format!(
                "attachments: {} holds something that is not a file; it is not replaced",
                dir.display()
            ));
        }
        std::fs::remove_file(&path).map_err(|e| io("an earlier file", &e))?;
    }
    std::fs::remove_dir(dir).map_err(|e| io("the directory", &e))
}

/// The day of an RFC 3339 time (`YYYY-MM-DD`).
fn day_of(now: &str) -> &str {
    now.get(..10).unwrap_or(now)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Places `items` read-only in the context directory; `now` is an RFC 3339 time for the "Date read" column of the index.
pub fn place(
    context: &ContextDir,
    run_id: &str,
    items: &[Attachment],
    now: &str,
) -> Result<Placed, String> {
    for a in items {
        if !valid_attachment_name(&a.name) || is_reserved_attachment_name(&a.name) {
            return Err(format!(
                "attachments: a name is not a plain file name or is one agentd keeps for its index ({} bytes)",
                a.name.len()
            ));
        }
    }
    let dir = context.path().join(DIR);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_dir() => {
            let marker = dir.join(MARKER);
            if std::fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file()) {
                retire(&dir)?;
            } else {
                return Err(format!(
                    "attachments: {} is already there and agentd did not make it",
                    dir.display()
                ));
            }
        }
        Ok(_) => {
            return Err(format!(
                "attachments: {} exists and is not a directory",
                dir.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io("the directory", &e)),
    }
    std::fs::create_dir(&dir).map_err(|e| io("the directory", &e))?;
    let result = write_all(&dir, run_id, items, now);
    if result.is_err() {
        // Only what this call made: the directory did not exist (or was agentd's own) before it.
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn write_all(dir: &Path, run_id: &str, items: &[Attachment], now: &str) -> Result<Placed, String> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| io("the directory", &e))?;
    let digest = sha256_hex(
        items
            .iter()
            .map(|a| a.sha256.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    write_new(&dir.join(MARKER), format!("{digest}\n").as_bytes())?;
    let mut placed = Vec::with_capacity(items.len());
    for a in items {
        let path = dir.join(&a.name);
        write_new(&path, a.content.as_bytes())?;
        placed.push(PlacedItem {
            name: a.name.clone(),
            sha256: sha256_hex(a.content.as_bytes()),
            bytes: a.content.len() as u64,
            path,
        });
    }
    let rows: Vec<_> = placed
        .iter()
        .map(|p| {
            json!({"name": p.name, "sha256": p.sha256, "bytes": p.bytes, "path": p.path.display().to_string(), "rel": format!("{DIR}/{}", p.name)})
        })
        .collect();
    let mut index =
        serde_json::to_string(&json!({"schema": INDEX_SCHEMA, "run": run_id, "items": rows}))
            .map_err(|e| format!("attachments: index: {e}"))?;
    index.push('\n');
    write_new(&dir.join(INDEX_JSON), index.as_bytes())?;
    let mut table = String::from(
        "| Origin | Date read | sha256 | Licence | Why it matters |\n| --- | --- | --- | --- | --- |\n",
    );
    for p in &placed {
        table.push_str(&format!(
            "| attached by you: {} | {} | {} | the user's own file | stored copy at {DIR}/{} |\n",
            p.name,
            day_of(now),
            p.sha256,
            p.name
        ));
    }
    write_new(&dir.join(INDEX_MD), table.as_bytes())?;
    Ok(Placed {
        dir: dir.to_path_buf(),
        items: placed,
    })
}
