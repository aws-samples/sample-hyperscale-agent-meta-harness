//! The context directory of a fresh session: `<agent_data>/context/`, outside the
//! work tree, so that nothing in it can reach the end commit, the snapshots, the zip or the pull request. It holds the original
//! prompt (`task.md`, byte for byte) and the digest of the earlier session (`fresh-session-<n>.md`, at most
//! [`harness_protocol::direct::DIGEST_BUDGET_BYTES`] bytes), for a new engine session to read. A copy of each digest is uploaded
//! under the run's `agents/direct/` prefix for audit; it is not delivered.
//!
//! agentd runs as root and the engine owns the work tree: the directory is created by root with mode 0755 and its files with
//! mode 0644, so the engine reads them and cannot change them. A path that already exists as a link, or as anything but a
//! directory, is refused.

use crate::s3::S3Objects;
use harness_protocol::api::internal::ProcessCredentials;
use harness_protocol::direct::s3 as keys;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The directory under the agent's data directory.
pub const CONTEXT_DIR: &str = "context";
/// The original prompt.
pub const TASK_FILE: &str = "task.md";
/// The audit upload is tried again this many times, with a pause that doubles.
pub const AUDIT_UPLOAD_RETRIES: u32 = 3;
/// The most bytes of the notes in a digest file besides the task ([`harness_protocol::direct::DIGEST_BUDGET_BYTES`]); the digest builder drops
/// whole sections to fit it.
pub use harness_protocol::direct::DIGEST_BUDGET_BYTES;

/// A digest file that was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestFile {
    pub path: PathBuf,
    pub bytes: u64,
    /// SHA-256 of the file's bytes, lower case hexadecimal (`DirectFreshSession.digest_sha256`).
    pub sha256: String,
}

/// `<agent_data>/context/`.
#[derive(Clone, Debug)]
pub struct ContextDir {
    dir: PathBuf,
}

fn io(e: std::io::Error) -> String {
    format!("context: {e}")
}

impl ContextDir {
    /// Creates the directory (root-owned, mode 0755) under `agent_data`; an existing directory of that name is reused and its mode
    /// put back to 0755. A link or a file of that name is refused.
    pub fn create(agent_data: &Path) -> Result<Self, String> {
        let dir = agent_data.join(CONTEXT_DIR);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "context: {} exists and is not a directory",
                    dir.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir).map_err(io)?;
            }
            Err(e) => return Err(io(e)),
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn task_path(&self) -> PathBuf {
        self.dir.join(TASK_FILE)
    }

    pub fn digest_path(&self, n: u32) -> PathBuf {
        self.dir.join(format!("fresh-session-{n}.md"))
    }

    /// Writes `name` in the directory with mode 0644: to a temporary name that is never followed if it is a link, then renamed over
    /// the final name, so a reader sees the old file or the new one and never half of one.
    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&tmp)
            .map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(io)?;
        drop(file);
        std::fs::rename(&tmp, &path).map_err(io)?;
        Ok(path)
    }

    /// Writes `task.md`: the prompt the run started with, byte for byte (mode 0644).
    pub fn write_task(&self, task: &str) -> Result<PathBuf, String> {
        self.write_file(TASK_FILE, task.as_bytes())
    }

    /// Writes `fresh-session-<n>.md` (mode 0644). A digest over the budget is refused: the builder of the digest drops whole
    /// sections to fit, so a longer text is a defect. The user's task is whole in the digest and outside the budget, so the limit is the
    /// budget and the size of `task.md`.
    pub fn write_digest(&self, n: u32, digest: &str) -> Result<DigestFile, String> {
        // The driver writes `task.md` before any digest. The heading of the task section and the cleaning of its text add a few bytes.
        let task = std::fs::metadata(self.dir.join(TASK_FILE)).map_or(0, |m| m.len() as usize);
        let limit = DIGEST_BUDGET_BYTES + if task > 0 { task + 256 } else { 0 };
        if digest.len() > limit {
            return Err(format!(
                "context: the digest is {} bytes, over the budget of {DIGEST_BUDGET_BYTES} bytes and the task",
                digest.len()
            ));
        }
        let path = self.write_file(&format!("fresh-session-{n}.md"), digest.as_bytes())?;
        Ok(DigestFile {
            path,
            bytes: digest.len() as u64,
            sha256: sha256_hex(digest.as_bytes()),
        })
    }
}

/// SHA-256 of `bytes`, lower case hexadecimal.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `s3://<bucket>/runs/<run>/agents/direct/turns/1/fresh-session-<n>.md`: where the audit copy of the digest goes.
pub fn audit_uri(bucket: &str, run_id: &str, n: u32) -> String {
    keys::uri(
        bucket,
        &format!("{}fresh-session-{n}.md", keys::turn_prefix(run_id)),
    )
}

/// PUTs the audit copy `file` to `uri`, trying again [`AUDIT_UPLOAD_RETRIES`] times with a pause that starts at `pause` and doubles.
pub async fn upload_audit_copy(
    s3: &S3Objects,
    uri: &str,
    creds: &ProcessCredentials,
    file: &Path,
    pause: Duration,
) -> Result<(), String> {
    let mut wait = pause;
    let mut last = String::new();
    for attempt in 0..=AUDIT_UPLOAD_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
        match s3.put_file(uri, creds, file).await {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}
