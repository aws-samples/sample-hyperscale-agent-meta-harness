//! Artifact download: the artifact list, a presigned URL, then 16 MiB ranged
//! GETs, up to four at a time and written in order, into `<file>.part` with an `<file>.part.etag`
//! sidecar (`ham_core::download`). A rerun resumes from the `.part` while the ETag is unchanged;
//! every ranged GET carries `If-Match`. The size and the SHA-256 are checked against the artifact
//! before the rename; a mismatch removes the `.part`. An expired URL (403) is replaced once.
//!
//! One download at a time writes the `.part` of a destination: it holds the lock file `<file>.part.lock` (flock, removed while still
//! locked) and, inside this process, a claim on the destination. A second download of the same artifact (the dashboard's automatic one
//! and Enter on the Artifacts screen, or two shells) waits for the first and then keeps the file the first verified; it resumes the
//! `.part` when the first one died, and ends with `PullError::Busy` when the first one's `.part` has not grown for the idle limit.
//! A directory that cannot lock files gets a download into its own unique file, from the first byte.

use crate::api::{Api, ApiError};
use crate::http::{ControlPlane, redact_url};
use ham_core::command::PullArtifact;
use ham_core::download::{
    CHUNK_BYTES, DownloadError, etag_path, lock_path, part_path, plan_ranges, resume_from,
};
use ham_core::render::{CardUsage, FinalCard};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, RunDetail};
use harness_protocol::store::ArtifactKind;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Ranged GETs in flight at once.
pub const PARALLEL_RANGES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pulled {
    pub path: PathBuf,
    pub artifact: ArtifactView,
    /// The presigned URL used last and its true expiry.
    pub url: DownloadUrlResponse,
    /// Ranges fetched by this call, in file order (empty when the file was already verified).
    pub ranges: Vec<(u64, u64)>,
    /// Bytes kept from an earlier `.part`.
    pub resumed_from: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("{0}")]
    NoArtifact(String),
    #[error("download: {0}")]
    Fetch(String),
    #[error("io: {0}")]
    Io(String),
    #[error("download: {0}")]
    Download(#[from] DownloadError),
    /// Another download holds the file and made no progress for the whole wait.
    #[error("{0}")]
    Busy(String),
}

fn io(e: std::io::Error) -> PullError {
    PullError::Io(e.to_string())
}

fn key_of(a: &ArtifactView) -> &str {
    a.s3_uri
        .strip_prefix("s3://")
        .and_then(|r| r.split_once('/'))
        .map_or("", |(_, k)| k)
}

fn kind_name(k: ArtifactKind) -> &'static str {
    match k {
        ArtifactKind::Zip => "zip",
        ArtifactKind::Partial => "partial",
        ArtifactKind::Bundle => "bundle",
        ArtifactKind::Evidence => "evidence",
        ArtifactKind::Log => "log",
    }
}

/// The artifact `--artifact` names: the newest of its kind (artifact ids are ULIDs or content-derived).
pub fn select(artifacts: &[ArtifactView], which: &PullArtifact) -> Result<ArtifactView, PullError> {
    let newest = |kind: ArtifactKind| {
        artifacts
            .iter()
            .filter(|a| a.kind == kind)
            .max_by(|a, b| a.artifact_id.cmp(&b.artifact_id))
            .cloned()
    };
    let found = match which {
        PullArtifact::Snapshot => {
            return Err(PullError::NoArtifact(
                "a snapshot is not an artifact; ham pull <runId> --snapshot downloads it".into(),
            ));
        }
        PullArtifact::Zip => newest(ArtifactKind::Zip),
        PullArtifact::Evidence => newest(ArtifactKind::Evidence),
        PullArtifact::Partial => newest(ArtifactKind::Partial),
    };
    found.ok_or_else(|| {
        let have: Vec<String> = artifacts
            .iter()
            .map(|a| format!("{} {}", kind_name(a.kind), a.artifact_id))
            .collect();
        PullError::NoArtifact(format!(
            "no matching artifact; the run has {}",
            if have.is_empty() {
                "none yet".to_string()
            } else {
                have.join(", ")
            }
        ))
    })
}

/// The local file name: the key's last segment when it is a plain name, else `<runId>-<id>.zip`.
pub fn local_name(run_id: &str, a: &ArtifactView) -> String {
    let base = key_of(a).rsplit('/').next().unwrap_or_default();
    let plain = !base.is_empty()
        && !base.starts_with('.')
        && base.len() <= 200
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain {
        base.to_string()
    } else {
        let ext = match a.kind {
            ArtifactKind::Zip | ArtifactKind::Partial | ArtifactKind::Evidence => "zip",
            ArtifactKind::Log => "jsonl",
            ArtifactKind::Bundle => "bin",
        };
        format!("{run_id}-{}.{ext}", a.artifact_id)
    }
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, PullError> {
    let mut f = std::fs::File::open(path).map_err(io)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

enum RangeError {
    /// 403: the URL expired or its credentials lapsed.
    Expired,
    Other(String),
}

/// One ranged GET with its checks; retried on network errors and 5xx.
async fn fetch_range<P: ControlPlane>(
    api: &Api<P>,
    url: &str,
    (start, end): (u64, u64),
    etag: &str,
    size: u64,
) -> Result<Vec<u8>, RangeError> {
    let retry = api.retry();
    let mut wait = retry.base;
    let mut attempt = 1;
    let what = format!("GET {} bytes={start}-{end}", redact_url(url));
    loop {
        let outcome = api.plane().fetch(url, Some((start, end)), Some(etag)).await;
        let transient = match &outcome {
            Ok(r) => r.status >= 500,
            Err(e) => e.is_retryable(),
        };
        if transient && attempt < retry.attempts.max(1) {
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2);
            attempt += 1;
            continue;
        }
        let r = outcome.map_err(|e| RangeError::Other(e.to_string()))?;
        return match r.status {
            206 => {
                let want = format!("bytes {start}-{end}/{size}");
                let got = r.headers.get("content-range").cloned().unwrap_or_default();
                if got != want {
                    Err(RangeError::Other(format!(
                        "{what}: Content-Range {got:?}, expected {want:?}"
                    )))
                } else if r.body.len() as u64 != end - start + 1 {
                    Err(RangeError::Other(format!(
                        "{what}: {} bytes, expected {}",
                        r.body.len(),
                        end - start + 1
                    )))
                } else {
                    Ok(r.body)
                }
            }
            403 => Err(RangeError::Expired),
            412 => Err(RangeError::Other(format!(
                "{what}: the object changed during the download; run ham pull again"
            ))),
            s => Err(RangeError::Other(format!("{what}: HTTP {s}"))),
        };
    }
}

/// What the probe of the object found.
struct Probe {
    size: u64,
    etag: String,
    /// The whole object, when the store answered the probe with it (HTTP 200).
    whole: Option<Vec<u8>>,
}

async fn probe<P: ControlPlane>(
    api: &Api<P>,
    url: &str,
    a: &ArtifactView,
) -> Result<Option<Probe>, PullError> {
    let r = api
        .plane()
        .fetch(url, Some((0, 0)), None)
        .await
        .map_err(|e| PullError::Fetch(e.to_string()))?;
    let etag = r.headers.get("etag").cloned().unwrap_or_default();
    match r.status {
        206 => {
            let size = r
                .headers
                .get("content-range")
                .and_then(|v| v.rsplit('/').next())
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| {
                    PullError::Fetch(format!(
                        "GET {}: no object size in Content-Range",
                        redact_url(url)
                    ))
                })?;
            Ok(Some(Probe {
                size,
                etag,
                whole: None,
            }))
        }
        200 => Ok(Some(Probe {
            size: r.body.len() as u64,
            etag,
            whole: Some(r.body),
        })),
        // An empty object has no byte 0.
        416 if a.size_bytes == 0 => Ok(Some(Probe {
            size: 0,
            etag,
            whole: Some(Vec::new()),
        })),
        403 => Ok(None),
        s => Err(PullError::Fetch(format!(
            "GET {}: HTTP {s}",
            redact_url(url)
        ))),
    }
}

/// Downloads `a` into `dir` (see the module doc). A file already at the destination with the
/// artifact's SHA-256 is kept and nothing is fetched.
pub async fn download<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    a: &ArtifactView,
    dir: &Path,
) -> Result<Pulled, PullError> {
    download_with(api, run_id, a, dir, CHUNK_BYTES).await
}

/// Range sizes `HAMH_RANGE_BYTES` may choose: 1 KiB up to the 16 MiB default.
pub const RANGE_BYTES: std::ops::RangeInclusive<u64> = 1024..=CHUNK_BYTES;

/// `HAMH_RANGE_BYTES`, else 16 MiB.
pub fn range_bytes(from_env: Option<&str>) -> Result<u64, String> {
    match from_env {
        None | Some("") => Ok(CHUNK_BYTES),
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| RANGE_BYTES.contains(n))
            .ok_or_else(|| {
                format!("HAMH_RANGE_BYTES takes 1024 to {CHUNK_BYTES} bytes, got {v:?}")
            }),
    }
}

/// [`download`] with `chunk`-byte ranges.
pub async fn download_with<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    a: &ArtifactView,
    dir: &Path,
    chunk: u64,
) -> Result<Pulled, PullError> {
    download_waiting(api, run_id, a, dir, chunk, LOCK_IDLE).await
}

/// How long a download waits for a file that another download holds, counted from the last time that download's `.part` grew.
pub const LOCK_IDLE: Duration = Duration::from_secs(300);

/// [`download_with`] that gives up on a held file after `idle` without progress. One download at a time writes the `.part` of a
/// destination: the others wait for it (they then keep the file it verified, or resume its `.part` when it died).
pub async fn download_waiting<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    a: &ArtifactView,
    dir: &Path,
    chunk: u64,
    idle: Duration,
) -> Result<Pulled, PullError> {
    std::fs::create_dir_all(dir).map_err(io)?;
    let dest = dir.join(local_name(run_id, a));
    // Held to the end of this function, so it is released after the last handle on the `.part` is closed.
    let lock = match lock_dest(&dest, idle).await? {
        Locked::Held(lock) => Some(lock),
        Locked::Unsupported => None,
    };
    let result = fetch(api, run_id, a, &dest, chunk, lock.is_some()).await;
    drop(lock);
    result
}

/// The download for a directory that cannot lock files: its own unique file, no resume, the other downloads' files left alone.
#[doc(hidden)]
pub async fn download_unlocked<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    a: &ArtifactView,
    dir: &Path,
    chunk: u64,
) -> Result<Pulled, PullError> {
    std::fs::create_dir_all(dir).map_err(io)?;
    let dest = dir.join(local_name(run_id, a));
    fetch(api, run_id, a, &dest, chunk, false).await
}

/// The destinations that a download of this process holds: flock does not tell two tasks of one process apart on every file system.
static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// This process's claim on a destination, released when dropped.
struct Claim(PathBuf);

impl Claim {
    fn take(key: &Path) -> Option<Claim> {
        let mut g = held();
        if g.iter().any(|k| k == key) {
            return None;
        }
        g.push(key.to_path_buf());
        Some(Claim(key.to_path_buf()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        held().retain(|k| k != &self.0);
    }
}

/// The lock file of a destination, locked by this download. It is removed while it is still locked (the last thing this download
/// does with the destination's files), so a download that opened it meanwhile finds another file at the path and starts over.
struct DestLock {
    file: File,
    path: PathBuf,
    _claim: Claim,
}

impl Drop for DestLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = self.file.unlock();
    }
}

enum Locked {
    Held(DestLock),
    /// The file system gives no locks (some network and FUSE mounts).
    Unsupported,
}

/// An error that says this file system cannot lock, not that someone else holds the lock.
fn lock_unsupported(e: &std::io::Error) -> bool {
    #[cfg(target_os = "linux")]
    const CODES: &[i32] = &[37, 38, 95]; // ENOLCK, ENOSYS, EOPNOTSUPP
    #[cfg(target_os = "macos")]
    const CODES: &[i32] = &[77, 78, 45, 102]; // ENOLCK, ENOSYS, ENOTSUP, EOPNOTSUPP
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const CODES: &[i32] = &[];
    e.kind() == std::io::ErrorKind::Unsupported
        || e.raw_os_error().is_some_and(|c| CODES.contains(&c))
}

/// What locking an opened lock file came to.
#[derive(Debug)]
enum Locking {
    Locked(File),
    /// Another holds it.
    Busy,
    /// The path no longer names the file that was opened: its holder removed it meanwhile, so a lock on it excludes nobody.
    Replaced,
}

/// Locks `file`, just opened from `path`, without waiting.
fn lock_opened(path: &Path, file: File) -> std::io::Result<Locking> {
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Locking::Busy),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let here = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(there) if there.dev() == here.dev() && there.ino() == here.ino() => {
            Ok(Locking::Locked(file))
        }
        Ok(_) => Ok(Locking::Replaced),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Locking::Replaced),
        Err(e) => Err(e),
    }
}

/// Opens (creating) and locks the file at `path` without waiting; `None` while another holds it.
fn try_flock(path: &Path) -> std::io::Result<Option<File>> {
    for _ in 0..64 {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match lock_opened(path, file)? {
            Locking::Locked(file) => return Ok(Some(file)),
            Locking::Busy => return Ok(None),
            Locking::Replaced => {}
        }
    }
    Ok(None)
}

/// The bytes of the destination's `.part`, 0 when there is none.
fn part_len(dest: &Path) -> u64 {
    std::fs::metadata(part_path(dest)).map_or(0, |m| m.len())
}

/// Takes the destination's lock, waiting while another download holds it. The wait ends with [`PullError::Busy`] when the holder's `.part`
/// has not grown for `idle`.
async fn lock_dest(dest: &Path, idle: Duration) -> Result<Locked, PullError> {
    let key = dest
        .parent()
        .and_then(|d| std::fs::canonicalize(d).ok())
        .zip(dest.file_name())
        .map_or_else(|| dest.to_path_buf(), |(d, n)| d.join(n));
    let lock = lock_path(dest);
    let mut seen = part_len(dest);
    let mut moved = Instant::now();
    let mut pause = Duration::from_millis(50);
    loop {
        if let Some(claim) = Claim::take(&key) {
            match try_flock(&lock) {
                Ok(Some(file)) => {
                    return Ok(Locked::Held(DestLock {
                        file,
                        path: lock,
                        _claim: claim,
                    }));
                }
                Ok(None) => {}
                Err(e) if lock_unsupported(&e) => return Ok(Locked::Unsupported),
                Err(e) => return Err(io(e)),
            }
        }
        let now = part_len(dest);
        if now != seen {
            seen = now;
            moved = Instant::now();
        }
        if moved.elapsed() >= idle {
            let name = dest.file_name().map_or_else(
                || dest.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            return Err(PullError::Busy(format!(
                "another download of {name} holds {} and has written nothing for {idle:?}; it may be stuck: end that ham process, then run this again",
                lock.display()
            )));
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(250));
    }
}

/// `actual` bytes are the artifact's size.
fn size_matches(actual: u64, a: &ArtifactView, what: &str) -> Result<(), PullError> {
    if actual == a.size_bytes {
        return Ok(());
    }
    Err(PullError::Fetch(format!(
        "{what} is {actual} bytes; the artifact lists {}",
        a.size_bytes
    )))
}

/// The file in place is the file that was verified. A different size now means a writer outside this protocol: the file is removed and
/// no Ok goes out.
fn confirm_in_place(dest: &Path, a: &ArtifactView) -> Result<(), PullError> {
    let now = std::fs::metadata(dest).map(|m| m.len()).map_err(io)?;
    size_matches(
        now,
        a,
        &format!("{} changed after it was verified and", dest.display()),
    )
    .inspect_err(|_| {
        let _ = std::fs::remove_file(dest);
    })
}

/// A file with the artifact's size and SHA-256 is at `dest`.
fn verified(dest: &Path, a: &ArtifactView) -> Result<bool, PullError> {
    match std::fs::metadata(dest) {
        Ok(m) if m.is_file() && m.len() == a.size_bytes => Ok(sha256_file(dest)? == a.sha256),
        _ => Ok(false),
    }
}

/// The SHA-256 of an open file from its first byte, read through the handle that wrote it.
fn sha256_open(f: &mut File) -> Result<String, PullError> {
    f.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A file that one download made for itself and did not rename into place: removed when dropped.
struct Scrap(Option<PathBuf>);

impl Drop for Scrap {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Downloads `a` to `dest`. With `shared` the caller holds the destination's lock: the bytes go to the resumable `<dest>.part` (and its
/// ETag file) and a destination that already verifies is kept and its leftovers removed. Without it they go to a unique file of this
/// download, from the first byte, and the files of other downloads are left alone.
async fn fetch<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    a: &ArtifactView,
    dest: &Path,
    chunk: u64,
    shared: bool,
) -> Result<Pulled, PullError> {
    let part = part_path(dest);
    let tag = etag_path(dest);
    let mut url = api.download_url(run_id, &a.artifact_id).await?;
    if verified(dest, a)? {
        if shared {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&tag);
        }
        return Ok(Pulled {
            path: dest.to_path_buf(),
            artifact: a.clone(),
            url,
            ranges: vec![],
            resumed_from: a.size_bytes,
        });
    }
    let mut refreshed = false;
    let found = loop {
        match probe(api, &url.url, a).await? {
            Some(p) => break p,
            None if !refreshed => {
                url = api.download_url(run_id, &a.artifact_id).await?;
                refreshed = true;
            }
            None => {
                return Err(PullError::Fetch(format!(
                    "GET {}: HTTP 403 with a fresh URL",
                    redact_url(&url.url)
                )));
            }
        }
    };
    if found.size != a.size_bytes {
        return Err(PullError::Fetch(format!(
            "the object is {} bytes; the artifact says {}",
            found.size, a.size_bytes
        )));
    }
    // Declared before the file, so the file is closed before this one removes it.
    let mut scrap = Scrap(None);
    let (work, from) = if shared {
        let part_len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let saved = std::fs::read_to_string(&tag).ok();
        let from = match &found.whole {
            Some(_) => 0,
            None => resume_from(part_len, saved.as_deref(), &found.etag, found.size),
        };
        (part.clone(), from)
    } else {
        let mut name = dest.as_os_str().to_owned();
        name.push(format!(".{}.part", crate::start::new_nonce()));
        let own = PathBuf::from(name);
        scrap.0 = Some(own.clone());
        (own, 0)
    };
    let mut file = if shared {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&work)
    } else {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&work)
    }
    .map_err(io)?;
    if shared {
        // The kept prefix, or nothing. The ETag is written after the cut: a crash in between never leaves a prefix of another object
        // under the ETag of this one.
        file.set_len(from).map_err(io)?;
        if from == 0 {
            std::fs::write(&tag, &found.etag).map_err(io)?;
        }
    }
    file.seek(SeekFrom::Start(from)).map_err(io)?;
    let mut fetched = Vec::new();
    if let Some(bytes) = &found.whole {
        file.write_all(bytes).map_err(io)?;
    } else {
        let ranges = plan_ranges(found.size, chunk, from);
        for window in ranges.chunks(PARALLEL_RANGES) {
            let mut results = fetch_window(api, &url.url, window, &found.etag, found.size).await;
            if results
                .iter()
                .any(|r| matches!(r, Err(RangeError::Expired)))
                && !refreshed
            {
                url = api.download_url(run_id, &a.artifact_id).await?;
                refreshed = true;
                results = fetch_window(api, &url.url, window, &found.etag, found.size).await;
            }
            // Write the leading run of successes so the `.part` stays a prefix of the object.
            for (range, result) in window.iter().zip(results) {
                match result {
                    Ok(bytes) => {
                        file.write_all(&bytes).map_err(io)?;
                        fetched.push(*range);
                    }
                    Err(RangeError::Expired) => {
                        file.flush().map_err(io)?;
                        return Err(PullError::Fetch(format!(
                            "GET {}: HTTP 403 (the presigned URL expired)",
                            redact_url(&url.url)
                        )));
                    }
                    Err(RangeError::Other(m)) => {
                        file.flush().map_err(io)?;
                        return Err(PullError::Fetch(m));
                    }
                }
            }
        }
    }
    file.flush().map_err(io)?;
    // Nothing is renamed into place unless it has the artifact's size and SHA-256, read through the handle that wrote it.
    let len = file.metadata().map_err(io)?.len();
    let bad = match size_matches(len, a, "the downloaded file") {
        Err(e) => Some(e),
        Ok(()) => {
            let actual = sha256_open(&mut file)?;
            (actual != a.sha256).then(|| {
                PullError::Download(DownloadError::Sha256Mismatch {
                    expected: a.sha256.clone(),
                    actual,
                })
            })
        }
    };
    if let Some(e) = bad {
        if shared {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&tag);
        }
        return Err(e);
    }
    file.sync_all().map_err(io)?;
    drop(file);
    std::fs::rename(&work, dest).map_err(io)?;
    scrap.0 = None;
    if shared {
        let _ = std::fs::remove_file(&tag);
    }
    confirm_in_place(dest, a)?;
    Ok(Pulled {
        path: dest.to_path_buf(),
        artifact: a.clone(),
        url,
        ranges: fetched,
        resumed_from: from,
    })
}

/// The window's ranges fetched at once; results in window order.
async fn fetch_window<P: ControlPlane>(
    api: &Api<P>,
    url: &str,
    window: &[(u64, u64)],
    etag: &str,
    size: u64,
) -> Vec<Result<Vec<u8>, RangeError>> {
    let mut set = tokio::task::JoinSet::new();
    for (i, range) in window.iter().copied().enumerate() {
        let (api, url, etag) = (api.clone(), url.to_string(), etag.to_string());
        set.spawn(async move { (i, fetch_range(&api, &url, range, &etag, size).await) });
    }
    let mut out: Vec<Option<Result<Vec<u8>, RangeError>>> = window.iter().map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((i, r)) = joined
            && let Some(slot) = out.get_mut(i)
        {
            *slot = Some(r);
        }
    }
    out.into_iter()
        .map(|r| r.unwrap_or_else(|| Err(RangeError::Other("a range task failed".into()))))
        .collect()
}

/// The artifact `which` names, downloaded into `dir`.
pub async fn pull<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    which: &PullArtifact,
    dir: &Path,
) -> Result<Pulled, PullError> {
    pull_with(api, run_id, which, dir, CHUNK_BYTES).await
}

/// [`pull`] with `chunk`-byte ranges.
pub async fn pull_with<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    which: &PullArtifact,
    dir: &Path,
    chunk: u64,
) -> Result<Pulled, PullError> {
    let list = api.list_artifacts(run_id).await?;
    let a = select(&list, which)?;
    download_with(api, run_id, &a, dir, chunk).await
}

/// What the stage reported a run used (client polish 2): the usage its turns recorded reaches the client as `cost`, priced by the
/// stage, so the tokens are the sum of its lines (the four kinds of token, as control-api counts them for a budget). `None` when the
/// stage reported none: the card then has no usage line and `--json` no `tokens` or `usd` member, never a fake 0. A model the price
/// table has no row for gives tokens and an unknown price.
pub fn run_usage(d: &RunDetail) -> Option<CardUsage> {
    let cost = d.cost.as_ref()?;
    let tokens = cost.lines.iter().fold(0u64, |sum, l| {
        let u = &l.usage;
        sum.saturating_add(u.input_tokens)
            .saturating_add(u.output_tokens)
            .saturating_add(u.cache_read_input_tokens)
            .saturating_add(u.cache_write_input_tokens)
    });
    if tokens == 0 {
        return None;
    }
    Some(CardUsage {
        tokens,
        usd: cost.unpriced_models.is_empty().then_some(d.cost_usd),
    })
}

/// The client facts of the final card: local path, presigned URL and expiry, tokens, USD.
pub fn card(detail: Option<&RunDetail>, pulled: Option<&Pulled>) -> FinalCard {
    FinalCard {
        local_path: pulled.map_or_else(
            || "not downloaded".to_string(),
            |p| p.path.display().to_string(),
        ),
        presigned_url: pulled.map(|p| p.url.url.clone()),
        expires_at: pulled.map(|p| p.url.expires_at.clone()),
        usage: detail.and_then(run_usage),
        label_missing_after: None,
        tests: None,
        flow: None,
        session: detail.and_then(|d| d.summary.session),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::ErrorKind;

    /// A fresh directory under the build's target directory.
    fn scratch(label: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/unit-scratch")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                crate::start::new_nonce()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(path: &Path) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_lock_on_the_file_at_the_path_is_taken_and_a_held_one_is_busy() {
        let dir = scratch("busy");
        let path = dir.join("x.zip.part.lock");
        let Locking::Locked(first) = lock_opened(&path, open(&path)).unwrap() else {
            panic!("the first lock should be taken");
        };
        let second = lock_opened(&path, open(&path)).unwrap();
        assert!(matches!(second, Locking::Busy), "{second:?}");
        drop(first);
        let third = lock_opened(&path, open(&path)).unwrap();
        assert!(matches!(third, Locking::Locked(_)), "{third:?}");
    }

    #[test]
    fn a_lock_on_a_file_that_its_holder_removed_does_not_count() {
        // The waiter opened the file, then the holder removed it and unlocked: the waiter locks a file nobody else will open.
        let dir = scratch("replaced");
        let path = dir.join("x.zip.part.lock");
        let opened = open(&path);
        std::fs::remove_file(&path).unwrap();
        let gone = lock_opened(&path, opened).unwrap();
        assert!(matches!(gone, Locking::Replaced), "{gone:?}");
        // The holder's successor made a new file at the path.
        let opened = open(&path);
        std::fs::remove_file(&path).unwrap();
        let _new = open(&path);
        let swapped = lock_opened(&path, opened).unwrap();
        assert!(matches!(swapped, Locking::Replaced), "{swapped:?}");
    }

    #[test]
    fn the_error_that_says_no_locks_here_is_told_from_the_one_that_says_someone_holds_it() {
        assert!(lock_unsupported(&std::io::Error::from(
            ErrorKind::Unsupported
        )));
        #[cfg(target_os = "linux")]
        for code in [37, 38, 95] {
            assert!(lock_unsupported(&std::io::Error::from_raw_os_error(code)));
        }
        #[cfg(target_os = "macos")]
        for code in [77, 78, 45, 102] {
            assert!(lock_unsupported(&std::io::Error::from_raw_os_error(code)));
        }
        for e in [
            std::io::Error::from(ErrorKind::WouldBlock),
            std::io::Error::from(ErrorKind::PermissionDenied),
            std::io::Error::from_raw_os_error(13),
            std::io::Error::from_raw_os_error(30),
        ] {
            assert!(!lock_unsupported(&e), "{e:?}");
        }
    }

    fn view(size: u64) -> ArtifactView {
        ArtifactView {
            artifact_id: "01X".into(),
            kind: ArtifactKind::Zip,
            s3_uri: "s3://b/runs/R/deliverable/app.zip".into(),
            sha256: "ab".repeat(32),
            size_bytes: size,
        }
    }

    #[test]
    fn a_file_of_another_size_than_the_artifact_lists_is_removed_and_never_reported() {
        let dir = scratch("size");
        let dest = dir.join("app.zip");
        std::fs::write(&dest, vec![7u8; 20]).unwrap();
        confirm_in_place(&dest, &view(20)).unwrap();
        assert!(dest.is_file(), "a file of the listed size stays");
        let e = confirm_in_place(&dest, &view(10)).unwrap_err();
        assert!(
            e.to_string().contains("20 bytes") && e.to_string().contains("lists 10"),
            "{e}"
        );
        // The same check before the rename.
        assert!(size_matches(10, &view(10), "the downloaded file").is_ok());
        let e = size_matches(9, &view(10), "the downloaded file").unwrap_err();
        assert_eq!(
            e.to_string(),
            "download: the downloaded file is 9 bytes; the artifact lists 10"
        );
        assert!(!dest.exists(), "the file of another size is removed");
    }

    #[test]
    fn the_wait_for_a_held_destination_is_one_claim_per_process() {
        let key = PathBuf::from("/nowhere/x.zip");
        let first = Claim::take(&key).unwrap();
        assert!(Claim::take(&key).is_none());
        assert!(Claim::take(Path::new("/nowhere/y.zip")).is_some());
        drop(first);
        assert!(Claim::take(&key).is_some());
    }
}
