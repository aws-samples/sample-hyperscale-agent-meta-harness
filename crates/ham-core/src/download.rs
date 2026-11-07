//! Resumable artifact download planning: 16 MiB ranged GETs into `<file>.part` with an
//! ETag sidecar, SHA-256 verified before the rename. The I/O lives in the client.

use crate::request::hex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const CHUNK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("sha256 mismatch: downloaded {actual}, object metadata says {expected}")]
    Sha256Mismatch { expected: String, actual: String },
}

/// Inclusive byte ranges from `from` to the end of an object of `size` bytes.
pub fn plan_ranges(size: u64, chunk: u64, from: u64) -> Vec<(u64, u64)> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut start = from;
    while start < size {
        let end = (start + chunk - 1).min(size - 1);
        out.push((start, end));
        start = end + 1;
    }
    out
}

fn with_suffix(dest: &Path, suffix: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn part_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part")
}

pub fn etag_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part.etag")
}

/// The lock file of a destination: the one download that holds it writes `<file>.part`; it is removed when that download ends.
pub fn lock_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part.lock")
}

/// Byte offset to resume from: the `.part` length when its saved ETag equals the object's and it is
/// not longer than the object; otherwise 0 (start over).
pub fn resume_from(part_len: u64, saved_etag: Option<&str>, remote_etag: &str, size: u64) -> u64 {
    match saved_etag {
        Some(e) if e == remote_etag && part_len <= size => part_len,
        _ => 0,
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn verify(bytes: &[u8], expected_sha256: &str) -> Result<(), DownloadError> {
    let actual = sha256_hex(bytes);
    if actual == expected_sha256 {
        Ok(())
    } else {
        Err(DownloadError::Sha256Mismatch {
            expected: expected_sha256.to_string(),
            actual,
        })
    }
}
