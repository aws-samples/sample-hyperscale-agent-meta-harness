//! Test support for the cost of the copies of the work: a stand-in for the bucket that never holds a body ([`bucket`]), files
//! of a given size that are never whole in memory, and the readings of a process that the bounds are held with.
#![allow(dead_code, clippy::unwrap_used)]

pub mod bucket;

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MIB: u64 = 1024 * 1024;

/// A file of the test's that is removed when the test ends, also when it fails: these tests write hundreds of megabytes.
pub struct TestFile(pub PathBuf);

impl TestFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One block of `block` bytes that do not compress, different for every `seed` (xorshift64).
pub fn noise(seed: u64, block: usize) -> Vec<u8> {
    let mut x = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(block + 8);
    while out.len() < block {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(block);
    out
}

/// Writes a file of `bytes` bytes to `dir/name`, one block of 1 MiB at a time (the file is never whole in memory). The blocks are one
/// block of bytes that do not compress, each with its own number in its first eight bytes, so no two blocks are equal.
pub fn big_file(dir: &Path, name: &str, bytes: u64) -> TestFile {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    let mut block = noise(0, usize::try_from(MIB).unwrap());
    let mut left = bytes;
    let mut n: u64 = 0;
    while left > 0 {
        block[..8].copy_from_slice(&n.to_le_bytes());
        let take = usize::try_from(left.min(MIB)).unwrap();
        file.write_all(&block[..take]).unwrap();
        left -= take as u64;
        n += 1;
    }
    file.flush().unwrap();
    TestFile(path)
}

/// The SHA-256 of the file at `path` in hexadecimal and its size, read one block at a time.
pub fn hash_of(path: &Path) -> (String, u64) {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut block = vec![0_u8; 1 << 20];
    let mut bytes = 0;
    loop {
        let n = file.read(&mut block).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&block[..n]);
        bytes += n as u64;
    }
    (hex(&hasher.finalize()), bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The processor time the calling thread has used (`CLOCK_THREAD_CPUTIME_ID`).
pub fn thread_cpu() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

/// The most memory this process has held at once, in bytes (`VmHWM` of `/proc/self/status`).
pub fn peak_memory() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .expect("VmHWM in /proc/self/status");
    let kib: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .expect("a number of kB");
    kib * 1024
}
