//! The confined extractor of the research module. A stored repository archive is a gzip tar that the
//! route wrote and listed in a manifest. This function unpacks it into a fresh directory only if it is exactly what the manifest says:
//!
//! - It never calls the `tar` crate's `unpack`. It reads the archive entry by entry and writes each file itself.
//! - Only regular files and directories are made. Any other kind of member ends the extraction.
//! - A path must be plain: relative, UTF-8, without `.`, `..`, empty segments, backslashes or control characters, at most 1,024 bytes.
//!   Because the manifest lists every file, a member it does not list ([`ExtractError::Unlisted`]), a listed file the archive lacks
//!   ([`ExtractError::Missing`]), a repeated member ([`ExtractError::Duplicate`]) and a hash or size that differs
//!   ([`ExtractError::Integrity`]) all end the extraction.
//! - The archive is read through a cap on the decoded bytes, each file through the single-file cap, and the number of members of any
//!   kind (files and directories) is capped. The decoded stream is read to its end, so bytes after the tar's end marker count against
//!   the cap too. Every sum and product is checked: a cap as large as a number can be does not wrap.
//!   The free space of the disk is checked before the first byte is written.
//! - Files are made with `create_new` and `O_NOFOLLOW` and end read-only with no execute or set-id bit (0444); directories end 0555.
//!   The destination must not exist, and a failed extraction removes what it made.

use crate::direct::research::settings::Settings;
use harness_protocol::research_copy::{sha256_hex, stored_normalised_sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The most bytes a path may have.
const MAX_PATH_BYTES: usize = 1024;
/// The most bytes a path segment may have.
const MAX_SEGMENT_BYTES: usize = 255;
/// What a tar member costs besides its data (header, padding, a long name or a pax record), for the cap on the decoded stream.
const PER_MEMBER_OVERHEAD: u64 = 2048;

/// The caps of one extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractCaps {
    /// All the unpacked file bytes.
    pub max_unpacked_bytes: u64,
    /// The most files.
    pub max_files: u32,
    /// One file.
    pub max_file_bytes: u64,
    /// Free space that must remain after the unpack.
    pub min_free_bytes: u64,
}

impl ExtractCaps {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            max_unpacked_bytes: s.max_unpacked_bytes,
            max_files: s.max_files,
            max_file_bytes: s.max_file_bytes,
            min_free_bytes: s.min_free_bytes,
        }
    }
}

impl Default for ExtractCaps {
    fn default() -> Self {
        Self::from_settings(&Settings::default())
    }
}

/// What the manifest says one file is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WantedFile {
    /// SHA-256 of the bytes, lower-case hexadecimal.
    pub sha256: String,
    pub bytes: u64,
}

/// A file that was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedFile {
    pub path: String,
    pub sha256: String,
    /// SHA-256 of the normalised bytes (`harness_protocol::research_copy::stored_normalised_sha256`); `None` when the normalised form
    /// is under the normalised floor, a file of blanks only included.
    pub norm_sha256: Option<String>,
    pub bytes: u64,
}

/// The result: the files in path order and their total size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub files: Vec<ExtractedFile>,
    pub unpacked_bytes: u64,
}

/// Why an extraction ended. Messages name a path with its characters escaped and never carry file content.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    #[error("the archive is not a readable gzip tar: {0}")]
    BadArchive(String),
    #[error("a path in the archive is not plain: {0:?}")]
    BadPath(String),
    #[error("the archive holds a member that is not a file or a directory: {0}")]
    Refused(String),
    #[error("the archive holds {0:?}, which the manifest does not list")]
    Unlisted(String),
    #[error("the manifest lists {0:?}, which the archive does not hold")]
    Missing(String),
    #[error("the archive holds {0:?} more than once")]
    Duplicate(String),
    #[error("{0:?} is not what the manifest says (size or SHA-256)")]
    Integrity(String),
    #[error("over a size cap: {0}")]
    TooLarge(String),
    #[error("more files than the cap allows")]
    TooMany,
    #[error("not enough free space for the unpack")]
    NoSpace,
    #[error("the destination exists")]
    Exists,
    #[error("{0}")]
    Io(String),
}

impl ExtractError {
    /// The code the API answers with (the route's own words where they exist).
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadArchive(_)
            | Self::BadPath(_)
            | Self::Refused(_)
            | Self::Unlisted(_)
            | Self::Missing(_)
            | Self::Duplicate(_)
            | Self::Integrity(_) => "integrity",
            Self::TooLarge(_) | Self::TooMany => "too_large",
            Self::NoSpace => "no_space",
            Self::Exists | Self::Io(_) => "unavailable",
        }
    }
}

fn io_err(e: &std::io::Error) -> ExtractError {
    ExtractError::Io(format!("extract: {}", e.kind()))
}

/// A reader that fails, and says so, when more than `left` bytes are asked of it: the cap on the decoded stream.
struct Capped<R> {
    inner: R,
    left: u64,
    hit: Arc<AtomicBool>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            // The stream may end exactly here; only more data than the cap allows is an error.
            let mut one = [0u8; 1];
            return match self.inner.read(&mut one)? {
                0 => Ok(0),
                _ => {
                    self.hit.store(true, Ordering::SeqCst);
                    Err(std::io::Error::other("the decoded stream passed its cap"))
                }
            };
        }
        let want = usize::try_from(self.left).map_or(buf.len(), |l| l.min(buf.len()));
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// `Ok(segments)` of a plain relative path, else the reason.
fn plain_path(raw: &[u8], directory: bool) -> Result<Vec<String>, ExtractError> {
    let bad = || ExtractError::BadPath(String::from_utf8_lossy(raw).into_owned());
    let text = std::str::from_utf8(raw).map_err(|_| bad())?;
    let text = if directory {
        text.strip_suffix('/').unwrap_or(text)
    } else {
        text
    };
    if text.is_empty() || text.len() > MAX_PATH_BYTES {
        return Err(bad());
    }
    let mut segments = Vec::new();
    for seg in text.split('/') {
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || seg.len() > MAX_SEGMENT_BYTES
            || seg.contains('\\')
            || seg.chars().any(|c| c.is_control())
        {
            return Err(bad());
        }
        segments.push(seg.to_string());
    }
    Ok(segments)
}

/// Makes `segments` under `root` one directory at a time. Only directories this call made or found are followed; a link or a file
/// in the way is an error.
fn ensure_dirs(root: &Path, segments: &[String]) -> Result<PathBuf, ExtractError> {
    let mut at = root.to_path_buf();
    for seg in segments {
        at.push(seg);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(ExtractError::BadPath(seg.clone())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&at).map_err(|e| io_err(&e))?;
            }
            Err(e) => return Err(io_err(&e)),
        }
    }
    Ok(at)
}

fn free_bytes(dir: &Path) -> Result<u64, ExtractError> {
    let st = rustix::fs::statvfs(dir).map_err(|e| ExtractError::Io(format!("statvfs: {e}")))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Gives a tree its write bits back and removes it. Used on a failed extraction and by tests.
pub fn remove_tree(root: &Path) {
    fn open_up(p: &Path) {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        if m.file_type().is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    open_up(&e.path());
                }
            }
        }
    }
    open_up(root);
    let _ = std::fs::remove_dir_all(root);
}

/// Makes every directory under `root` (and `root`) 0555, deepest first.
fn seal_dirs(root: &Path) -> Result<(), ExtractError> {
    fn walk(p: &Path) -> Result<(), ExtractError> {
        for e in std::fs::read_dir(p).map_err(|e| io_err(&e))? {
            let e = e.map_err(|e| io_err(&e))?;
            if e.file_type().map_err(|e| io_err(&e))?.is_dir() {
                walk(&e.path())?;
            }
        }
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o555)).map_err(|e| io_err(&e))
    }
    walk(root)
}

/// Unpacks `archive` (a gzip tar) into the new directory `dest` as `wanted` lists it. On any error nothing is left at `dest`.
pub fn extract_targz(
    archive: &[u8],
    dest: &Path,
    wanted: &BTreeMap<String, WantedFile>,
    caps: &ExtractCaps,
) -> Result<Extracted, ExtractError> {
    // What the manifest alone can refuse, before any byte is written.
    if wanted.len() as u64 > u64::from(caps.max_files) {
        return Err(ExtractError::TooMany);
    }
    let listed: u64 = wanted
        .values()
        .try_fold(0u64, |sum, w| sum.checked_add(w.bytes))
        .ok_or_else(|| {
            ExtractError::TooLarge("the manifest lists more bytes than a number holds".into())
        })?;
    if listed > caps.max_unpacked_bytes {
        return Err(ExtractError::TooLarge(format!(
            "the manifest lists {listed} bytes, over the cap of {}",
            caps.max_unpacked_bytes
        )));
    }
    if let Some((p, w)) = wanted.iter().find(|(_, w)| w.bytes > caps.max_file_bytes) {
        return Err(ExtractError::TooLarge(format!(
            "{p:?} is {} bytes, over the file cap of {}",
            w.bytes, caps.max_file_bytes
        )));
    }
    match std::fs::symlink_metadata(dest) {
        Ok(_) => return Err(ExtractError::Exists),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&e)),
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    if free_bytes(parent)? < listed.saturating_add(caps.min_free_bytes) {
        return Err(ExtractError::NoSpace);
    }
    std::fs::create_dir(dest).map_err(|e| io_err(&e))?;
    match extract_into(archive, dest, wanted, caps) {
        Ok(done) => Ok(done),
        Err(e) => {
            remove_tree(dest);
            Err(e)
        }
    }
}

fn extract_into(
    archive: &[u8],
    dest: &Path,
    wanted: &BTreeMap<String, WantedFile>,
    caps: &ExtractCaps,
) -> Result<Extracted, ExtractError> {
    let hit = Arc::new(AtomicBool::new(false));
    // A tree has at most one directory entry per file, so twice the file cap bounds the members of any honest archive.
    let max_members = u64::from(caps.max_files)
        .saturating_mul(2)
        .saturating_add(16);
    let budget = caps
        .max_unpacked_bytes
        .saturating_add(max_members.saturating_mul(PER_MEMBER_OVERHEAD))
        .saturating_add(4096);
    let decoder = Capped {
        inner: flate2::read::GzDecoder::new(archive),
        left: budget,
        hit: hit.clone(),
    };
    let read_failed = |e: std::io::Error| {
        if hit.load(Ordering::SeqCst) {
            ExtractError::TooLarge("the archive expands past the cap".to_string())
        } else {
            ExtractError::BadArchive(e.kind().to_string())
        }
    };
    let mut tar = tar::Archive::new(decoder);
    let entries = tar.entries().map_err(&read_failed)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut done: BTreeMap<String, ExtractedFile> = BTreeMap::new();
    let mut total: u64 = 0;
    let mut members: u64 = 0;
    for entry in entries {
        let mut entry = entry.map_err(&read_failed)?;
        members += 1;
        if members > max_members {
            return Err(ExtractError::TooMany);
        }
        let kind = entry.header().entry_type();
        let directory = kind.is_dir();
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }
        if !(kind.is_file() || directory) {
            return Err(ExtractError::Refused(format!("{kind:?}")));
        }
        let segments = plain_path(&entry.path_bytes(), directory)?;
        if directory {
            ensure_dirs(dest, &segments)?;
            continue;
        }
        let path = segments.join("/");
        if !seen.insert(path.clone()) {
            return Err(ExtractError::Duplicate(path));
        }
        let Some(want) = wanted.get(&path) else {
            return Err(ExtractError::Unlisted(path));
        };
        let size = entry.header().size().map_err(|e| io_err(&e))?;
        if size > caps.max_file_bytes {
            return Err(ExtractError::TooLarge(format!(
                "{path:?} is {size} bytes, over the file cap of {}",
                caps.max_file_bytes
            )));
        }
        if size != want.bytes {
            return Err(ExtractError::Integrity(path));
        }
        total = total.saturating_add(size);
        if total > caps.max_unpacked_bytes {
            return Err(ExtractError::TooLarge(format!(
                "the files pass the cap of {} unpacked bytes",
                caps.max_unpacked_bytes
            )));
        }
        let mut body = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(1 << 20));
        entry
            .by_ref()
            .take(size.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(&read_failed)?;
        if body.len() as u64 != size {
            return Err(ExtractError::BadArchive("a member is cut short".into()));
        }
        let sha256 = sha256_hex(&body);
        if sha256 != want.sha256 {
            return Err(ExtractError::Integrity(path));
        }
        let (name, dirs) = segments
            .split_last()
            .ok_or_else(|| ExtractError::BadPath(path.clone()))?;
        let parent = ensure_dirs(dest, dirs)?;
        let target = parent.join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&target)
            .map_err(|e| io_err(&e))?;
        std::io::Write::write_all(&mut file, &body).map_err(|e| io_err(&e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o444))
            .map_err(|e| io_err(&e))?;
        done.insert(
            path.clone(),
            ExtractedFile {
                path,
                sha256,
                norm_sha256: stored_normalised_sha256(&body),
                bytes: size,
            },
        );
    }
    if let Some(missing) = wanted.keys().find(|k| !seen.contains(*k)) {
        return Err(ExtractError::Missing(missing.clone()));
    }
    // Whatever follows the tar's end marker is read through the same cap, so a stream that goes on past what the manifest lists fails.
    let mut rest = tar.into_inner();
    std::io::copy(&mut rest, &mut std::io::sink()).map_err(&read_failed)?;
    seal_dirs(dest)?;
    Ok(Extracted {
        files: done.into_values().collect(),
        unpacked_bytes: total,
    })
}
