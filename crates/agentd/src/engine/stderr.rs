//! The engine's stderr log: rotated, never truncated, and read through the handle agentd made.
//!
//! `File::create` truncated `engine-stderr.log` at the start of every turn, so the last words of a turn that died at
//! start-up were gone when the retry began (the cause of the failed turns had to be inferred from command lines). A captured spawn moves the old log to
//! `engine-stderr.prev.log` (one is kept) and opens a fresh one. The engine's uid owns the directory, so it can unlink the
//! log or put a link at its path while it runs; agentd, which runs as root, therefore never reads the path afterwards. It
//! keeps a second handle on the file it created and reads the end of that, so a link to another file shows nothing of it.

use harness_core::engine_diagnostic::STDERR_WINDOW_BYTES;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Where the previous log of `log` goes: `engine-stderr.log` becomes `engine-stderr.prev.log`, `x.stderr` becomes
/// `x.prev.stderr`, a name with no extension gets `.prev`.
pub fn prev_stderr_path(log: &Path) -> PathBuf {
    let name = log.file_name().and_then(|n| n.to_str()).unwrap_or("stderr");
    let prev = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.prev.{ext}"),
        _ => format!("{name}.prev"),
    };
    log.with_file_name(prev)
}

/// Moves `log` to its previous-log name, replacing the one there. Best effort: a log that is not there is fine, and a
/// failure leaves [`open_fresh`] to deal with what is in the way. Says whether something was moved.
pub(crate) fn rotate(log: &Path) -> bool {
    std::fs::rename(log, prev_stderr_path(log)).is_ok()
}

/// Puts the log that [`rotate`] moved aside back at `log`, over the empty one [`open_fresh`] opened. For a start that failed
/// before the engine ran (a working directory that is gone, an exec that cannot happen): nothing new was written, and the
/// words of the last real run must stay the log instead of an empty file taking their place.
pub(crate) fn restore_rotated(log: &Path) {
    let _ = std::fs::rename(prev_stderr_path(log), log);
}

/// A new, empty log for the engine's stderr, opened for reading too (agentd reads its end later) and for appending (the
/// engine's writes always land at the end, whatever it seeks to). It never follows a link at `log`: a name that is still
/// taken after the rotation (a link or a file the engine put there, or a directory) is removed once and tried again.
pub(crate) fn open_fresh(log: &Path) -> io::Result<File> {
    let open = || {
        OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(log)
    };
    match open() {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(log)?;
            open()
        }
        other => other,
    }
}

/// What [`StderrCapture::tail`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTail {
    /// The last bytes of the log, at most [`STDERR_WINDOW_BYTES`].
    pub bytes: Vec<u8>,
    /// The log held more than `bytes`.
    pub cut: bool,
}

/// agentd's own handle on the engine's stderr log.
#[derive(Debug)]
pub struct StderrCapture {
    file: File,
}

impl StderrCapture {
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }

    /// The end of the log through this handle (`pread`, so the engine's write offset is untouched), however the path has
    /// been changed since.
    pub fn tail(&self) -> io::Result<RawTail> {
        let len = self.file.metadata()?.len();
        let start = len.saturating_sub(STDERR_WINDOW_BYTES as u64);
        let mut bytes = vec![0u8; usize::try_from(len - start).unwrap_or(STDERR_WINDOW_BYTES)];
        let mut got = 0;
        while got < bytes.len() {
            match self.file.read_at(&mut bytes[got..], start + got as u64) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        bytes.truncate(got);
        Ok(RawTail {
            bytes,
            cut: start > 0,
        })
    }
}
