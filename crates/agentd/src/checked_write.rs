//! A checked write of a small file the worker itself puts on the session's storage.
//!
//! The session's storage can refuse the data of a write with errno 28 while its files leave room.
//! The write call itself then works: the refusal comes at the sync or at the close, and the file is
//! left empty. A write that checks neither does not see it. The worker's write of `.git/config` at the end of a session was such a
//! write: a refusal left the file empty, and the run ended as a repository that had changed.
//!
//! [`replace`] is the one write that sees a refusal: the bytes go to a new file beside the target, the file is synced, its close is
//! checked, and only then is it renamed over the target. A refused write leaves the target as it stood. The git settings the end of a
//! session puts back (`crate::workspace::GitState`) and the marker of the copies (`crate::direct::marker`) are written with it.
//!
//! [`FileCalls`] is the five file calls of that write. The host's calls are the trait's own methods ([`Host`]); a test answers one of
//! them as the storage was seen to.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// "No space left on device".
pub const ENOSPC: i32 = 28;

/// The step of a checked write that failed (`create`, `write`, `sync`, `close` or `rename`) with the system's error.
pub type Failed = (&'static str, std::io::Error);

/// The steps of a write of the worker's own file at which errno 28 is the storage's refusal of the data.
pub const DATA_STEPS: [&str; 5] = ["create", "write", "sync", "close", "rename"];

/// The one rule of a refused write: errno 28 at one of [`DATA_STEPS`]. The storage watch's own test write and the
/// checked write read it here.
pub fn refused_at(step: &str, errno: Option<i32>) -> bool {
    errno == Some(ENOSPC) && DATA_STEPS.contains(&step)
}

/// Whether a step of [`replace`] that failed is the storage's refusal of the data: errno 28.
pub fn refused(failed: &Failed) -> bool {
    refused_at(failed.0, failed.1.raw_os_error())
}

/// The file calls of one write. Every method is the host's call unless a stand-in replaces it.
pub trait FileCalls: Send + Sync + std::fmt::Debug {
    /// A new plain file at `path` with the mode `mode`: made with `O_EXCL` and `O_NOFOLLOW`, so a name that exists is an error and
    /// no link is followed.
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    /// All of `bytes` written to `file`.
    fn write(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
        file.write_all(bytes)
    }

    /// The file's data and its size sent to the storage.
    fn sync(&self, file: &File) -> std::io::Result<()> {
        file.sync_all()
    }

    /// The file closed, with what the close answered. The descriptor is gone either way: a close is never tried twice.
    fn close(&self, file: File) -> std::io::Result<()> {
        nix::unistd::close(file.into_raw_fd()).map_err(std::io::Error::from)
    }

    /// `from` renamed to `to`. A file or a link at `to` is replaced, and a link there is not followed.
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }
}

/// The host's file calls.
#[derive(Clone, Copy, Debug, Default)]
pub struct Host;

impl FileCalls for Host {}

/// Writes `bytes` as the file `target` so that a write the storage refuses is seen: the bytes go to the new file `beside` (a name in
/// the target's folder that must not exist), the file is synced, its close is checked, and it is renamed over `target`. The first
/// step that fails is the answer, and the target then stands as it stood: the file beside it is taken away again.
///
/// The caller decides what may stand at the two names before the call. Nothing here follows a link: the new file is made with
/// `O_EXCL` and `O_NOFOLLOW`, and the rename replaces a link at `target` itself.
pub fn replace(
    calls: &dyn FileCalls,
    beside: &Path,
    target: &Path,
    mode: u32,
    bytes: &[u8],
) -> Result<(), Failed> {
    let mut file = calls.create(beside, mode).map_err(|e| ("create", e))?;
    let written = calls
        .write(&mut file, bytes)
        .map_err(|e| ("write", e))
        .and_then(|()| calls.sync(&file).map_err(|e| ("sync", e)));
    // The close ends the descriptor whatever came before it. What the write or the sync met comes first.
    let closed = calls.close(file).map_err(|e| ("close", e));
    let done = written
        .and(closed)
        .and_then(|()| calls.rename(beside, target).map_err(|e| ("rename", e)));
    if done.is_err() {
        // Nobody reads the file beside the target: it goes, and the target stays as it was.
        let _ = std::fs::remove_file(beside);
        return done;
    }
    // The new name is on the storage once the folder is synced. A folder that cannot be opened for that does not undo the write.
    if let Some(dir) = target.parent()
        && let Ok(folder) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
    {
        let _ = folder.sync_all();
    }
    Ok(())
}
