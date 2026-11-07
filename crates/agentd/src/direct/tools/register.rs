//! The engine's registration file: the settings file the engine reads its tool servers from, which agentd
//! writes once it has bound the server's port and writes again when the engine damages it.
//!
//! Where it is and what it says come from the engine's profile (`[mcp]`, `harness_protocol::engine_profile::EngineMcp`): the path, under the engine's
//! own `{dataDir}` or `{configDir}`, and a template with two placeholders, `{toolsName}` and `{toolsUrl}`. This file knows nothing about an engine.
//!
//! The file lives in a directory the engine owns, because the pinned engine rewrites its own state next to it after every connect.
//! agentd runs as root
//! and the engine, with its shell, can plant a link at any name in that directory, so nothing here follows one:
//!
//! - the root the profile names (the engine's data or configuration directory, which agentd made under a directory of its own that only root can change)
//!   is opened once, at startup, before the engine runs ([`Target::open_root`]), and the handle is kept: no later step resolves a path by name from
//!   above it. The directories below it are walked with `openat` and `O_NOFOLLOW | O_DIRECTORY`, one component at a time, and every later step is
//!   relative to the handle of the last one, so a directory swapped for a link after the walk changes nothing;
//! - the new content is written to a temporary name in that directory with `O_CREAT | O_EXCL | O_NOFOLLOW` and put in place with `renameat`, which
//!   replaces whatever is at the name (a file, a link, an empty directory) and never opens it;
//! - the stale files the engine's lock leaves (`<file>.lock*`, `<file>.tmp.*`) are removed with `unlinkat`, which unlinks a link and does not follow it.
//!
//! The engine can still write what it likes into the file between two of agentd's checks. That is not a boundary: it owns the file and has a shell.
//! The point of the rules above is that agentd, which is root, never writes through anything the engine chose.
//!
//! Errors are short words with no path and no content, for a log line.

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, mkdirat, openat, renameat, unlinkat,
};
use rustix::io::Errno;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

/// The temporary name the new content is written to, next to the file.
const TMP_SUFFIX: &str = ".agentd-new";
/// A file larger than the rendered text cannot be the rendered text; it is not read.
const MAX_READ: u64 = 64 * 1024;
/// The rendered text is at most this long, so that a registration the module wrote is never longer than it will read back.
pub const MAX_RENDERED: usize = 64 * 1024;
/// A pass over the engine's directory looks at this many entries at most: the directory belongs to the engine, which can fill it.
const MAX_SCAN: usize = 4096;

/// Where the registration file is: a root directory (the engine's data or configuration directory), the directories below it and the file's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    root: PathBuf,
    dirs: Vec<String>,
    name: String,
}

impl Target {
    /// The path of the file, for a person reading a test. Never opened by following it.
    pub fn display_path(&self) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(&self.dirs);
        p.join(&self.name)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The handle of the root directory, opened without following a link. Opened once, before the engine runs, and used for every later write and
    /// restore.
    pub fn open_root(&self) -> Result<OwnedFd, String> {
        openat(CWD, self.root.as_path(), dir_flags(), Mode::empty())
            .map_err(|e| format!("root: {}", word(e)))
    }
}

/// A part of a path the profile gave: a plain name.
fn plain(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains(['{', '}', '/', '\\', '\0'])
        && !part.chars().any(char::is_control)
}

/// Resolves the profile's `path` (`{dataDir}/settings/<file>` or `{configDir}/...`) against the run's directories. Anything else is refused: another
/// placeholder, an absolute path, a part that is empty or `.` or `..`. The profile loader refuses the same; this is the second check, at the place
/// that writes.
pub fn target(path: &str, data_dir: &Path, config_dir: &Path) -> Result<Target, String> {
    let (root, rest) = if let Some(r) = path.strip_prefix("{dataDir}/") {
        (data_dir, r)
    } else if let Some(r) = path.strip_prefix("{configDir}/") {
        (config_dir, r)
    } else {
        return Err("path".to_string());
    };
    let mut parts: Vec<&str> = rest.split('/').collect();
    if !parts.iter().all(|p| plain(p)) {
        return Err("path".to_string());
    }
    let name = parts.pop().ok_or_else(|| "path".to_string())?.to_string();
    Ok(Target {
        root: root.to_path_buf(),
        dirs: parts.into_iter().map(str::to_string).collect(),
        name,
    })
}

/// Fills the template: `{toolsName}` and `{toolsUrl}`. The result must be one JSON document, and no placeholder may be left in it.
pub fn render(template: &str, name: &str, url: &str) -> Result<String, String> {
    let text = template
        .replace("{toolsName}", name)
        .replace("{toolsUrl}", url);
    if text.contains("{tools") {
        return Err("placeholder".to_string());
    }
    if text.len() > MAX_RENDERED {
        return Err("template-too-large".to_string());
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| "template-not-json".to_string())?;
    Ok(text)
}

/// What a restore did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The file differed (it was edited, gone, a link, not a regular file, owned by someone else or unreadable to its owner) and was replaced.
    /// A file that was right is not written again and its modification time stays.
    pub rewritten: bool,
    /// How many stale lock and temporary files were removed (only when the engine was known to be gone).
    pub stale_removed: usize,
}

fn word(e: Errno) -> String {
    format!("{:?}", std::io::Error::from(e).kind()).to_ascii_lowercase()
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// The handle of the directory that holds the file, walked without following a link. With `create` a missing directory below the root is made
/// (0755, handed to `owner`); the root itself must exist.
fn open_dir(
    t: &Target,
    root: &OwnedFd,
    create: bool,
    owner: Option<(u32, u32)>,
) -> Result<OwnedFd, String> {
    let mut cur = root
        .try_clone()
        .map_err(|e| format!("root: {:?}", e.kind()).to_ascii_lowercase())?;
    for d in &t.dirs {
        cur = match openat(&cur, d.as_str(), dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if create => {
                mkdirat(&cur, d.as_str(), Mode::from_bits_truncate(0o755))
                    .map_err(|e| format!("mkdir: {}", word(e)))?;
                let fd = openat(&cur, d.as_str(), dir_flags(), Mode::empty())
                    .map_err(|e| format!("directory: {}", word(e)))?;
                give(&fd, owner)?;
                fd
            }
            Err(e) => return Err(format!("directory: {}", word(e))),
        };
    }
    Ok(cur)
}

/// Hands the open file or directory to `owner`, when it is not already theirs.
fn give(fd: &impl AsFd, owner: Option<(u32, u32)>) -> Result<(), String> {
    let Some((uid, gid)) = owner else {
        return Ok(());
    };
    if fstat(fd).is_ok_and(|st| st.st_uid == uid && st.st_gid == gid) {
        return Ok(());
    }
    std::os::unix::fs::fchown(fd, Some(uid), Some(gid))
        .map_err(|e| format!("chown: {:?}", e.kind()).to_ascii_lowercase())
}

/// Removes `name` from `dir`: a file or a link is unlinked (a link is never followed), an empty directory is removed. `false` when it could not be
/// removed (a directory that is not empty).
fn remove_entry(dir: &impl AsFd, name: &[u8]) -> bool {
    let Ok(cname) = std::ffi::CString::new(name) else {
        return false;
    };
    match unlinkat(dir, cname.as_c_str(), AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => true,
        Err(Errno::ISDIR | Errno::PERM) => {
            unlinkat(dir, cname.as_c_str(), AtFlags::REMOVEDIR).is_ok()
        }
        Err(_) => false,
    }
}

/// Removes what the engine's lock leaves next to the file: `<file>.lock*` (its lock library makes a directory) and `<file>.tmp.*`.
fn clean_stale(dir: &OwnedFd, name: &str) -> usize {
    let (lock, tmp) = (format!("{name}.lock"), format!("{name}.tmp."));
    let Ok(listing) = Dir::read_from(dir) else {
        return 0;
    };
    let stale: Vec<Vec<u8>> = listing
        .filter_map(Result::ok)
        .take(MAX_SCAN)
        .map(|e| e.file_name().to_bytes().to_vec())
        .filter(|n| n.starts_with(lock.as_bytes()) || n.starts_with(tmp.as_bytes()))
        .collect();
    stale.iter().filter(|n| remove_entry(dir, n)).count()
}

/// Writes `text` to the file and gives it (and its directory) to `owner`. The stale lock and temporary files are removed first. Returns how many.
pub fn write(
    t: &Target,
    root: &OwnedFd,
    text: &str,
    owner: Option<(u32, u32)>,
) -> Result<usize, String> {
    let dir = open_dir(t, root, true, owner)?;
    give(&dir, owner)?;
    let removed = clean_stale(&dir, &t.name);
    put(&dir, &t.name, text, owner)?;
    Ok(removed)
}

/// Compares the file with `text` and replaces it when it differs, is gone, is a link, is not a regular file, belongs to someone else or cannot be
/// read and written by its owner. A file that is right is not touched (its modification time stays). With `engine_gone` (its process group was
/// killed) the stale lock and temporary files are removed too: a lock left by a killed engine costs the next session's first prompt up to 9.5 s,
/// while a lock of an engine that still runs belongs to it and is left alone.
pub fn restore(
    t: &Target,
    root: &OwnedFd,
    text: &str,
    owner: Option<(u32, u32)>,
    engine_gone: bool,
) -> Result<Restored, String> {
    let dir = open_dir(t, root, true, owner)?;
    let stale_removed = if engine_gone {
        clean_stale(&dir, &t.name)
    } else {
        0
    };
    if is_current(&dir, &t.name, text.as_bytes(), owner) {
        return Ok(Restored {
            rewritten: false,
            stale_removed,
        });
    }
    give(&dir, owner)?;
    put(&dir, &t.name, text, owner)?;
    Ok(Restored {
        rewritten: true,
        stale_removed,
    })
}

/// Whether `name` in `dir` is a regular file (not a link) that holds exactly `want`, belongs to `owner` and can be read and written by it. The
/// open does not block on a FIFO and does not follow a link.
fn is_current(dir: &OwnedFd, name: &str, want: &[u8], owner: Option<(u32, u32)>) -> bool {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let Ok(fd) = openat(dir, name, flags, Mode::empty()) else {
        return false;
    };
    let Ok(st) = fstat(&fd) else {
        return false;
    };
    let size_ok = u64::try_from(st.st_size).is_ok_and(|s| s == want.len() as u64 && s <= MAX_READ);
    let owner_ok = owner.is_none_or(|(uid, gid)| st.st_uid == uid && st.st_gid == gid);
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || !size_ok
        || !owner_ok
        || st.st_mode & 0o600 != 0o600
    {
        return false;
    }
    let mut got = Vec::with_capacity(want.len());
    std::fs::File::from(fd)
        .take(MAX_READ)
        .read_to_end(&mut got)
        .is_ok()
        && got == want
}

/// Puts `text` at `name` in `dir`: a temporary name, created new and never through a link, then `renameat` over the name.
fn put(dir: &OwnedFd, name: &str, text: &str, owner: Option<(u32, u32)>) -> Result<(), String> {
    let tmp = format!("{name}{TMP_SUFFIX}");
    // Whatever the engine left at the temporary name goes first (unlinked, not followed).
    if !remove_entry(dir, tmp.as_bytes()) {
        return Err("temporary-name".to_string());
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat(dir, tmp.as_str(), flags, Mode::from_bits_truncate(0o644))
        .map_err(|e| format!("create: {}", word(e)))?;
    let mut file = std::fs::File::from(fd);
    let written = file
        .write_all(text.as_bytes())
        .map_err(|e| format!("write: {:?}", e.kind()).to_ascii_lowercase())
        .and_then(|()| give(&file, owner));
    drop(file);
    if let Err(e) = written {
        remove_entry(dir, tmp.as_bytes());
        return Err(e);
    }
    let mut renamed = renameat(dir, tmp.as_str(), dir, name);
    // The engine put a directory at the name: an empty one is removed, then the rename goes through.
    if matches!(renamed, Err(Errno::ISDIR | Errno::NOTEMPTY | Errno::EXIST))
        && remove_entry(dir, name.as_bytes())
    {
        renamed = renameat(dir, tmp.as_str(), dir, name);
    }
    renamed.map_err(|e| {
        remove_entry(dir, tmp.as_bytes());
        format!("rename: {}", word(e))
    })
}
