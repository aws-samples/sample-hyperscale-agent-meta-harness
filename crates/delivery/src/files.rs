//! File digests and per-invocation scratch directories.

use crate::error::DeliveryError;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// SHA-256 (hex) and size of a file, read in 1 MiB chunks.
pub fn sha256_file(path: &Path) -> Result<(String, u64), DeliveryError> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&h.finalize()), size))
}

/// A scratch directory under `root`, removed when dropped (Lambda keeps `/tmp` across warm
/// invocations).
pub fn scratch(root: &Path, prefix: &str) -> Result<tempfile::TempDir, DeliveryError> {
    std::fs::create_dir_all(root)?;
    Ok(tempfile::Builder::new().prefix(prefix).tempdir_in(root)?)
}
