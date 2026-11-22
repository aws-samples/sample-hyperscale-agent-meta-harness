//! The marker of the copies on the session's storage.
//!
//! A session that is stopped from outside leaves its files on its storage, and the service reads them back with a new machine
//! (`crate::direct::recover`). Whether those files are at or after the last saved copy is not something a time stamp can say. So agentd
//! writes down on the storage itself which copy the storage stands at: `<workspace_root>/<run>/copies.json`, a [`CopyMarker`]. It is
//! written right after a session's restore check passed, naming the commit the session started on, and right after the bundle of each
//! copy is stored, naming that copy. The service starts a session from the files read back only when the marker names exactly the saved
//! copy it compares them with.
//!
//! The file is in the run's folder, beside the engine's folder `direct/` and outside it. agentd makes the run's folder itself and hands
//! only `direct/` to the engine's user, so the engine cannot write the marker. The reader ([`read`]) takes the marker only when the file
//! and its folder belong to the trusted user (uid 0 in a session's machine) and neither is writable by group or others, and it opens the
//! file without following a link. A marker that is missing or not trusted is no marker: the copy is still made and stored, and the
//! service does not start a session from it.
//!
//! A marker that cannot be written never fails a copy: the caller logs it, and the next read finds the marker of an earlier copy, which
//! does not name the newest saved copy, so the run goes on from the saved copy.

use crate::direct::snapshot::TREE_ANSWER_LIMIT;
use harness_protocol::direct::CopyMarker;
use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The file name of the marker, in the run's folder on the session's storage.
pub const MARKER_FILE: &str = "copies.json";
/// The file a marker is written to before it is renamed over [`MARKER_FILE`].
const MARKER_TMP: &str = ".copies.json.tmp";
/// `schemaVersion` of the marker this agentd writes and reads.
pub const SCHEMA_VERSION: u32 = 1;
/// The user the marker and its folder must belong to in a session's machine: agentd runs there as root.
pub const TRUSTED_OWNER: u32 = 0;
/// The mode the marker is created with: its owner writes it, nobody else does. The reader's rule is that neither group nor others may
/// write; the exact mode is not laid down by the design and is checked against a real session's storage before a release.
const MARKER_MODE: u32 = 0o644;
/// The bits that let somebody but the owner write.
const WRITABLE_BY_OTHERS: u32 = 0o022;
/// The most bytes read from a marker; one is about 120 bytes.
const MAX_MARKER_BYTES: u64 = 4096;

/// One writer at a time. A write that outlives its caller's wait (a storage that answered late) ends before the next one starts, so
/// no writer renames another writer's file and the marker that stands is the one written last.
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the marker of `run` is on the storage at `workspace_root`.
pub fn path(workspace_root: &Path, run: &str) -> PathBuf {
    workspace_root.join(run).join(MARKER_FILE)
}

/// Writes `marker` as the marker of `run`: a new file beside it, synced, its close checked, then renamed over it
/// (`crate::checked_write::replace`), so a reader finds the earlier marker or this one and never half of one. The file calls are on the session's storage, which can stop answering: they run on a thread of their
/// own and are waited for [`TREE_ANSWER_LIMIT`] at the most.
pub async fn write(workspace_root: &Path, run: &str, marker: &CopyMarker) -> Result<(), String> {
    let dir = workspace_root.join(run);
    let bytes = serde_json::to_vec(marker).map_err(|e| format!("marker: {e}"))?;
    let work = tokio::task::spawn_blocking(move || {
        let _one = WRITING.lock().unwrap_or_else(|p| p.into_inner());
        write_now(&dir, &bytes)
    });
    match tokio::time::timeout(TREE_ANSWER_LIMIT, work).await {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => Err(format!("marker: {e}")),
        Err(_) => Err(format!(
            "marker: the storage did not answer within {} s",
            TREE_ANSWER_LIMIT.as_secs()
        )),
    }
}

fn io(what: &str, e: &std::io::Error) -> String {
    format!("marker: {what}: {e}")
}

/// The marker write for the watch of the session's storage (`crate::direct::storage_watch`), which reads the error's number:
/// one writer at a time, as in [`write`], and the step that failed with its error. The caller is on a thread that may wait for the disk.
pub(crate) fn write_locked(dir: &Path, bytes: &[u8]) -> Result<(), (&'static str, std::io::Error)> {
    let _one = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    write_steps(dir, bytes)
}

fn write_now(dir: &Path, bytes: &[u8]) -> Result<(), String> {
    write_steps(dir, bytes).map_err(|(what, e)| io(what, &e))
}

fn write_steps(dir: &Path, bytes: &[u8]) -> Result<(), (&'static str, std::io::Error)> {
    write_with(&crate::checked_write::Host, dir, bytes)
}

/// The marker's write with its file calls given, so that a test can answer one as the session's storage did. The bytes are
/// written beside the marker, the file is synced, its close is checked and it is renamed over the marker
/// (`crate::checked_write::replace`): a write the storage refuses is an error with its step, and the earlier marker stands as it
/// stood. The folder is synced after the rename; a folder that cannot be opened for that does not undo the marker.
pub fn write_with(
    calls: &dyn crate::checked_write::FileCalls,
    dir: &Path,
    bytes: &[u8],
) -> Result<(), (&'static str, std::io::Error)> {
    let tmp = dir.join(MARKER_TMP);
    // A file that a write which never ended left behind is replaced. The new file is made with `O_EXCL` and follows no link.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(("the earlier temporary file", e)),
    }
    crate::checked_write::replace(calls, &tmp, &dir.join(MARKER_FILE), MARKER_MODE, bytes)
}

/// The marker of `run` on the storage at `workspace_root`, as the worker of a read takes it: `None` unless the run's folder is a real
/// folder (no link) that belongs to `owner` and that neither group nor others may write, and the marker is a regular file of at most
/// [`MAX_MARKER_BYTES`] with the same owner and the same rule, opened without following a link, that parses as a [`CopyMarker`] of this
/// schema version with a session number and a commit id. `owner` is [`TRUSTED_OWNER`] in a session's machine.
pub fn read(workspace_root: &Path, run: &str, owner: u32) -> Option<CopyMarker> {
    use std::os::unix::fs::MetadataExt;
    let dir = workspace_root.join(run);
    let folder = std::fs::symlink_metadata(&dir).ok()?;
    if !folder.file_type().is_dir()
        || folder.uid() != owner
        || folder.mode() & WRITABLE_BY_OTHERS != 0
    {
        return None;
    }
    let fd = open(
        dir.join(MARKER_FILE),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let st = fstat(&fd).ok()?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || st.st_uid != owner
        || st.st_mode & WRITABLE_BY_OTHERS != 0
        || u64::try_from(st.st_size).ok()? > MAX_MARKER_BYTES
    {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_MARKER_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let marker: CopyMarker = serde_json::from_slice(&bytes).ok()?;
    let commit = marker.head_sha.len() == 40
        && marker
            .head_sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (marker.schema_version == SCHEMA_VERSION && marker.session >= 1 && commit).then_some(marker)
}
