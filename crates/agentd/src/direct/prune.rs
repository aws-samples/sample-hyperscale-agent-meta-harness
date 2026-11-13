//! Stale cargo build output is pruned while a prompt runs.
//!
//! The cargo target folder is on the session's root volume, a few GiB free at the start.
//! One test binary of a large Rust workspace can take several GiB and the whole workspace tens of GiB, and a changed feature set, profile or test selection
//! leaves a second copy of a unit beside the first. Nothing removed them, and a run ended with `the session disk is full`. This module is a
//! periodic look inside the direct run that frees room before that:
//!
//! * [`start`] wakes every `AGENTD_DISK_CHECK_S` seconds ([`tick`]), reads the free space of the root volume and, when it is below
//!   `AGENTD_DISK_PRUNE_MB`, prunes the target folder ([`prune_target`]) and reports what it freed once ([`notice`], a `notify` event).
//! * What the bookkeeping of cargo 1.98.1 shows (measured 2026-10-26 on stable cargo, three experiments): the file
//!   `.fingerprint/<name>-<hash>/invoked.timestamp` is touched when a unit is compiled and NOT when a later build finds it fresh. So an old
//!   timestamp says "not compiled lately", and a dependency that every build still needs shows the same. A source edit does not change the hash
//!   (a rebuild overwrites the same files); a second copy of a unit comes from another feature set. The rules follow:
//!   1. a unit compiled inside the keep window (`AGENTD_CARGO_KEEP_MIN`, 20 minutes) is never removed, and neither is a hash that a unit
//!      inside the window, or a unit with no readable timestamp or `.json` file, also carries;
//!   2. first tier: an older variant, a unit whose name and target (the `.json` file of its fingerprint folder) another unit has with a newer
//!      timestamp;
//!   3. second tier, only when the volume is still below the stop mark (`AGENTD_DISK_LOW_MB`) after the first: the other old units, which the
//!      next build compiles again. This is the rule as it was specified (remove what no build of the last minutes used) and costs a rebuild.
//! * A unit is its fingerprint folder, the files of its hash in `deps/` (a name that ends in `-<16 hex digits>` and has no extension or one
//!   extension) and its folder `build/<name>-<hash>`. The fingerprint folder goes last, so a prune that was cut short leaves a unit that the
//!   next one finds again.
//! * Safety. agentd is root and the engine's user owns the folder, so nothing is reached by a path twice: the folder is opened without following
//!   a link and everything below it is opened and removed relative to a directory handle, one entry at a time, never across a filesystem. A link
//!   is never followed (it is removed as a link when it sits inside a unit that goes, and skipped when it stands where a folder belongs). A running
//!   cargo holds an exclusive lock on the lock files of its profile directory (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`); the
//!   prune takes the same locks without waiting, skips a profile directory whose lock is held (the next look tries again) and keeps the locks until
//!   it is done, so a cargo that starts meanwhile waits. A profile directory with no lock file is skipped: a missing lock shows no build.
//! * The bytes are an estimate (allocated blocks of the files whose only link was removed), not the space the volume gained.

use crate::config::DirectConfig;
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat, statat,
    unlinkat,
};
use rustix::io::Errno;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How deep a unit's folder may go before the removal stops descending (a cargo `build/` folder is four levels at most).
const MAX_DEPTH: u32 = 32;
/// The most entries of the target folder read to find the profile directories.
const MAX_PROFILE_SCAN: usize = 4096;

/// What a prune did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Units removed (their fingerprint folders are gone).
    pub units: u32,
    /// Estimated bytes freed: the allocated blocks of the removed files that had no other link.
    pub bytes: u64,
    /// Profile directories skipped because a cargo build held their lock.
    pub busy: u32,
}

/// Who reads the free space of the root volume. The real one is [`StatvfsFree`]; the tests give a scripted one.
pub trait FreeProbe: Send + Sync + std::fmt::Debug {
    /// Free space in MiB that an unprivileged writer can use; `None` when it could not be read.
    fn free_mb(&self) -> Option<u64>;
}

/// The real probe: `statvfs` of a folder of the root volume (the data root, `/var/lib/agentd`).
#[derive(Debug)]
pub struct StatvfsFree(PathBuf);

impl StatvfsFree {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

impl FreeProbe for StatvfsFree {
    fn free_mb(&self) -> Option<u64> {
        crate::direct::disk::free_mb(&self.0)
    }
}

/// The settings of the pruning: all four come from the stage's environment, none from a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneSettings {
    /// `AGENTD_DISK_CHECK_S`: the time between two looks.
    pub every: Duration,
    /// `AGENTD_DISK_PRUNE_MB`: prune when the root volume has less free space than this.
    pub below_mb: u64,
    /// `AGENTD_CARGO_KEEP_MIN`: a unit compiled inside this time is never removed.
    pub keep: Duration,
    /// `AGENTD_DISK_LOW_MB`: the stop mark; below it after the first tier, the second tier runs. 0: it never does.
    pub low_mb: u64,
}

impl PruneSettings {
    /// The settings of a run, or `None` when the pruning is switched off: a look interval, a mark or a keep time of 0.
    pub fn from_config(d: &DirectConfig) -> Option<Self> {
        (!d.disk_check.is_zero() && d.disk_prune_mb > 0 && !d.cargo_keep.is_zero()).then_some(
            Self {
                every: d.disk_check,
                below_mb: d.disk_prune_mb,
                keep: d.cargo_keep,
                low_mb: d.disk_low_mb,
            },
        )
    }
}

// ---- the directory handles ---------------------------------------------------------------------------------------------------

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// A folder under `parent`, opened without following a link; `None` for a link, a file, a missing entry or any error.
fn open_dir(parent: &impl AsFd, name: &OsStr) -> Option<OwnedFd> {
    openat(parent, name, dir_flags(), Mode::empty()).ok()
}

/// The entries of an open folder (not `.` and `..`) with the kind the directory read gave; a filesystem that gives none reads `Unknown`.
fn list(dir: &impl AsFd) -> Vec<(OsString, FileType)> {
    let Ok(entries) = Dir::read_from(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_bytes();
            (name != b"." && name != b"..")
                .then(|| (OsStr::from_bytes(name).to_os_string(), e.file_type()))
        })
        .collect()
}

fn is_dir_at(parent: &impl AsFd, name: &OsStr, hint: FileType) -> bool {
    match hint {
        FileType::Directory => true,
        FileType::Unknown => statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory),
        _ => false,
    }
}

/// A number of a `stat` as `u64`. The field types differ between x86-64 and arm64 (`st_nlink` is 64 bits on one and 32 on the other), so a plain
/// `u64::from` would be a useless conversion on one of them.
fn n64<T: TryInto<u64>>(value: T) -> u64 {
    value.try_into().unwrap_or(0)
}

/// Removes `name` under `parent` and returns the estimated bytes freed and whether the entry is gone: a folder with everything in it, any other
/// kind of entry (a link included) itself. A link is never followed and an entry on another filesystem than `dev` is left.
fn remove_at(parent: &impl AsFd, name: &OsStr, dev: u64, depth: u32) -> (u64, bool) {
    let Ok(st) = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return (0, false);
    };
    if n64(st.st_dev) != dev {
        return (0, false);
    }
    let blocks = n64(st.st_blocks).saturating_mul(512);
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        if depth >= MAX_DEPTH {
            return (0, false);
        }
        let Some(fd) = open_dir(parent, name) else {
            return (0, false);
        };
        // The folder that was opened is the folder that was looked at: a name swapped in between is another entry.
        if fstat(&fd).map_or(true, |s| s.st_ino != st.st_ino || n64(s.st_dev) != dev) {
            return (0, false);
        }
        let mut bytes = 0u64;
        for (child, _) in list(&fd) {
            bytes = bytes.saturating_add(remove_at(&fd, &child, dev, depth + 1).0);
        }
        drop(fd);
        if unlinkat(parent, name, AtFlags::REMOVEDIR).is_ok() {
            (bytes.saturating_add(blocks), true)
        } else {
            (bytes, false)
        }
    } else {
        let freed = if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
            && n64(st.st_nlink) == 1
        {
            blocks
        } else {
            0
        };
        if unlinkat(parent, name, AtFlags::empty()).is_ok() {
            (freed, true)
        } else {
            (0, false)
        }
    }
}

// ---- units -------------------------------------------------------------------------------------------------------------------

/// `<name>-<16 lower-case hexadecimal digits>`: the name of a fingerprint folder and of a `build/` folder. Returns the name and the hash.
fn split_hash(entry: &str) -> Option<(&str, &str)> {
    let (name, hash) = entry.rsplit_once('-')?;
    (!name.is_empty()
        && hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some((name, hash))
}

/// Whether the file `name` of `deps/` belongs to the unit with `hash`: its name up to the first dot ends in `-<hash>`, and what follows the dot, if
/// anything, is one extension (`rlib`, `rmeta`, `d`, `so`, `o`), not several (`x.txt.bak`).
fn deps_file_has_hash(name: &OsStr, hash: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let (stem, ext) = match name.split_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (name, None),
    };
    if ext
        .is_some_and(|e| e.is_empty() || !e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    {
        return false;
    }
    split_hash(stem).is_some_and(|(_, h)| h == hash)
}

/// One fingerprint folder.
struct Unit {
    /// The name of the folder, which is also the name of its `build/` folder.
    entry: OsString,
    hash: String,
    /// The target this unit builds and how: its name, the name of the one `.json` file of its folder and the fields of that file that do not
    /// change with the feature set (`rustc`, `target`, `profile`, `path`, `rustflags`, `config`, `compile_kind`). Two units with the same value
    /// are variants of one another. `None` when the folder holds no such file or several, or the file cannot be read as cargo writes it.
    ident: Option<String>,
    /// When `invoked.timestamp` was last touched; `None` when there is no such regular file.
    stamp: Option<SystemTime>,
}

fn read_unit(fp: &impl AsFd, entry: OsString) -> Option<Unit> {
    let text = entry.to_str()?;
    let (name, hash) = split_hash(text)?;
    let (name, hash) = (name.to_string(), hash.to_string());
    // A link where the folder belongs is not opened.
    let dir = open_dir(fp, &entry)?;
    let stamp = openat(
        &dir,
        "invoked.timestamp",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .and_then(|fd| File::from(fd).metadata().ok())
    .filter(std::fs::Metadata::is_file)
    .and_then(|m| m.modified().ok());
    let jsons: Vec<String> = list(&dir)
        .into_iter()
        .filter_map(|(n, _)| {
            n.to_str()
                .and_then(|s| s.strip_suffix(".json"))
                .map(str::to_string)
        })
        .collect();
    let ident = (jsons.len() == 1)
        .then(|| fingerprint_ident(&dir, &name, &jsons[0]))
        .flatten();
    Some(Unit {
        entry,
        hash,
        ident,
        stamp,
    })
}

/// The fields of a fingerprint file that name a target and how it is built, apart from its feature set. Measured on cargo 1.98.1: of two units of
/// one target built with another feature set only `features` and the unit's own paths differ. Two versions of a crate differ in `path`, a host
/// and a target copy in `compile_kind`, another profile in `profile`.
const IDENT_FIELDS: [&str; 7] = [
    "rustc",
    "target",
    "profile",
    "path",
    "rustflags",
    "config",
    "compile_kind",
];

/// The most bytes read of a fingerprint file (they are a few hundred).
const MAX_FINGERPRINT_BYTES: u64 = 256 * 1024;

fn fingerprint_ident(unit: &OwnedFd, name: &str, stem: &str) -> Option<String> {
    use std::io::Read;
    let fd = openat(
        unit,
        format!("{stem}.json"),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    let mut text = Vec::new();
    file.metadata()
        .ok()
        .filter(std::fs::Metadata::is_file)?
        .len()
        .le(&MAX_FINGERPRINT_BYTES)
        .then_some(())?;
    file.take(MAX_FINGERPRINT_BYTES)
        .read_to_end(&mut text)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&text).ok()?;
    let mut key = format!("{name}\u{0}{stem}");
    for field in IDENT_FIELDS {
        key.push('\u{0}');
        key.push_str(&value.get(field)?.to_string());
    }
    Some(key)
}

/// A profile directory with its cargo locks taken.
struct Profile {
    fp: OwnedFd,
    deps: Option<OwnedFd>,
    /// `examples/`: the files of an example carry the hash of its unit like the ones in `deps/`.
    examples: Option<OwnedFd>,
    build: Option<OwnedFd>,
    /// The locks, held until the profile is dropped.
    _locks: Vec<OwnedFd>,
    units: Vec<Unit>,
    removed: BTreeSet<usize>,
}

enum Opened {
    /// Not a profile directory, or one that cannot be pruned safely (no lock file, a lock that cannot be taken).
    Skip,
    /// A cargo build holds a lock.
    Busy,
    Ready(Box<Profile>),
}

/// Takes the cargo locks of a profile directory without waiting and reads its units.
fn open_profile(dir: &OwnedFd) -> Opened {
    let Some(fp) = open_dir(dir, OsStr::new(".fingerprint")) else {
        return Opened::Skip;
    };
    let mut locks = Vec::new();
    for (name, _) in list(dir) {
        let Some(text) = name.to_str() else { continue };
        if !(text.starts_with(".cargo-") && text.ends_with("lock")) {
            continue;
        }
        // A link is not opened; a regular lock file is opened for reading only (the lock does not need more) and never created.
        let Ok(fd) = openat(
            dir,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            continue;
        };
        match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => locks.push(fd),
            Err(Errno::WOULDBLOCK) => return Opened::Busy,
            Err(_) => return Opened::Skip,
        }
    }
    if locks.is_empty() {
        return Opened::Skip;
    }
    let units = list(&fp)
        .into_iter()
        .filter(|(name, hint)| is_dir_at(&fp, name, *hint))
        .filter_map(|(name, _)| read_unit(&fp, name))
        .collect();
    Opened::Ready(Box::new(Profile {
        deps: open_dir(dir, OsStr::new("deps")),
        examples: open_dir(dir, OsStr::new("examples")),
        build: open_dir(dir, OsStr::new("build")),
        fp,
        _locks: locks,
        units,
        removed: BTreeSet::new(),
    }))
}

impl Profile {
    /// The indexes of the units that may go: compiled longer ago than `keep`, with a readable `.json` file, and a hash that no unit that stays
    /// carries. Each with the stamp, in the order of the folder.
    fn candidates(&self, now: SystemTime, keep: Duration) -> Vec<usize> {
        let old = |u: &Unit| {
            u.ident.is_some()
                && u.stamp
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|age| age > keep)
        };
        let protected: BTreeSet<&str> = self
            .units
            .iter()
            .enumerate()
            .filter(|(i, u)| !self.removed.contains(i) && !old(u))
            .map(|(_, u)| u.hash.as_str())
            .collect();
        self.units
            .iter()
            .enumerate()
            .filter(|(i, u)| {
                !self.removed.contains(i) && old(u) && !protected.contains(u.hash.as_str())
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The candidates that another unit of the same name and target has a newer timestamp over.
    fn superseded(&self, candidates: &[usize]) -> Vec<usize> {
        let mut newest: BTreeMap<&str, SystemTime> = BTreeMap::new();
        for u in &self.units {
            if let (Some(ident), Some(stamp)) = (&u.ident, u.stamp) {
                let e = newest.entry(ident.as_str()).or_insert(stamp);
                *e = (*e).max(stamp);
            }
        }
        candidates
            .iter()
            .copied()
            .filter(|i| {
                let u = &self.units[*i];
                match (&u.ident, u.stamp) {
                    (Some(ident), Some(stamp)) => {
                        newest.get(ident.as_str()).is_some_and(|n| *n > stamp)
                    }
                    _ => false,
                }
            })
            .collect()
    }

    /// Removes the units at `which`: the files of their hashes in `deps/`, their `build/` folders, and last their fingerprint folders.
    fn remove(&mut self, which: &[usize], dev: u64) -> (u32, u64) {
        if which.is_empty() {
            return (0, 0);
        }
        // The files that can carry a unit's hash: `deps/` and `examples/`, listed once for all the units of this removal.
        let hashed: Vec<(&OwnedFd, Vec<OsString>)> = [self.deps.as_ref(), self.examples.as_ref()]
            .into_iter()
            .flatten()
            .map(|d| {
                let names = list(d)
                    .into_iter()
                    .filter(|(_, hint)| *hint != FileType::Directory)
                    .map(|(n, _)| n)
                    .collect();
                (d, names)
            })
            .collect();
        let (mut units, mut bytes) = (0u32, 0u64);
        for &i in which {
            let hash = self.units[i].hash.clone();
            for (d, names) in &hashed {
                for name in names.iter().filter(|n| deps_file_has_hash(n, &hash)) {
                    bytes = bytes.saturating_add(remove_at(*d, name, dev, 0).0);
                }
            }
            if let Some(b) = &self.build {
                bytes = bytes.saturating_add(remove_at(b, &self.units[i].entry, dev, 0).0);
            }
            let (freed, gone) = remove_at(&self.fp, &self.units[i].entry, dev, 0);
            bytes = bytes.saturating_add(freed);
            if gone {
                units += 1;
                self.removed.insert(i);
            }
        }
        (units, bytes)
    }
}

/// The profile directories of a target folder: the directories below it that hold `.fingerprint/` (`debug`, `release`, a custom profile) and the
/// ones one level deeper (a cross target, `<triple>/debug`). A link is not entered.
fn profile_dirs(root: &OwnedFd) -> Vec<OwnedFd> {
    let has_fp = |fd: &OwnedFd| {
        statat(fd, ".fingerprint", AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory)
    };
    let mut found = Vec::new();
    let mut scanned = 0usize;
    for (name, hint) in list(root) {
        if name.as_bytes().starts_with(b".") || !is_dir_at(root, &name, hint) {
            continue;
        }
        let Some(first) = open_dir(root, &name) else {
            continue;
        };
        if has_fp(&first) {
            found.push(first);
            continue;
        }
        for (inner, inner_hint) in list(&first) {
            scanned += 1;
            if scanned > MAX_PROFILE_SCAN {
                return found;
            }
            if inner.as_bytes().starts_with(b".") || !is_dir_at(&first, &inner, inner_hint) {
                continue;
            }
            if let Some(second) = open_dir(&first, &inner)
                && has_fp(&second)
            {
                found.push(second);
            }
        }
    }
    found
}

/// Whether a process runs a program that sits under `root` (`/proc/<pid>/exe`). agentd is root and reads the links of every process.
fn runs_from(root: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs.filter_map(Result::ok).any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read_link(e.path().join("exe")).is_ok_and(|exe| exe.starts_with(&root))
    })
}

/// Prunes the cargo target folder `root` as the module's rules say and returns what it did. `now` and `keep` fix which units are old;
/// `still_short` is asked once, after the first tier of every profile directory, and says whether the root volume is still short of room (below
/// the stop mark): only then does the second tier run. A missing folder, a link, or one that cannot be opened prunes nothing.
pub fn prune_target(
    root: &Path,
    now: SystemTime,
    keep: Duration,
    still_short: &dyn Fn() -> bool,
) -> Pruned {
    let mut out = Pruned::default();
    let Ok(top) = openat(CWD, root, dir_flags(), Mode::empty()) else {
        return out;
    };
    let Ok(st) = fstat(&top) else {
        return out;
    };
    let dev = n64(st.st_dev);
    // A test run starts its binaries from `deps/` after cargo has given its locks back: nothing goes while a program runs from the folder.
    if runs_from(root) {
        out.busy += 1;
        return out;
    }
    let mut profiles = Vec::new();
    for dir in profile_dirs(&top) {
        match open_profile(&dir) {
            Opened::Ready(p) => profiles.push(p),
            Opened::Busy => out.busy += 1,
            Opened::Skip => {}
        }
    }
    let mut add = |(units, bytes): (u32, u64)| {
        out.units += units;
        out.bytes = out.bytes.saturating_add(bytes);
    };
    for p in &mut profiles {
        let first = {
            let candidates = p.candidates(now, keep);
            p.superseded(&candidates)
        };
        add(p.remove(&first, dev));
    }
    if !profiles.is_empty() && still_short() {
        for p in &mut profiles {
            let rest = p.candidates(now, keep);
            add(p.remove(&rest, dev));
        }
    }
    out
}

// ---- the look that runs with the run -------------------------------------------------------------------------------------------

/// One look: reads the free space and, when it is below the prune mark, prunes. `Some` only when a unit was removed: the prune with the free
/// space read after it. The work runs on a blocking thread.
pub async fn tick(
    settings: &PruneSettings,
    target: &Path,
    free: Arc<dyn FreeProbe>,
    now: SystemTime,
) -> Option<(Pruned, Option<u64>)> {
    let before = free.free_mb()?;
    if before >= settings.below_mb {
        return None;
    }
    let (target, keep, low) = (target.to_path_buf(), settings.keep, settings.low_mb);
    let probe = free.clone();
    let pruned = tokio::task::spawn_blocking(move || {
        prune_target(&target, now, keep, &|| {
            low > 0 && probe.free_mb().is_some_and(|f| f < low)
        })
    })
    .await
    .ok()?;
    (pruned.units > 0).then(|| (pruned, free.free_mb()))
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{} KiB", bytes >> 10)
    }
}

/// The words of the run's event for a prune: the units, the estimated size and the free space after it, when it could be read.
pub fn notice(pruned: &Pruned, free_after: Option<u64>) -> String {
    let units = if pruned.units == 1 { "unit" } else { "units" };
    let mut text = format!(
        "Pruned stale cargo build output: {} {units}, about {} freed.",
        pruned.units,
        size_text(pruned.bytes)
    );
    if let Some(free) = free_after {
        text.push_str(&format!(" The root volume has {free} MiB free."));
    }
    text
}

/// What the task does with a prune that removed something.
pub type Report = Arc<dyn Fn(&Pruned, Option<u64>) + Send + Sync>;

/// The look that runs while the run does. Stopping it waits for the prune it is in; dropping it ends it at once.
#[derive(Debug)]
pub struct PruneTask {
    handle: tokio::task::JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl PruneTask {
    /// Stops the look and waits until it is over. A prune that is running works on a blocking thread, which cannot be aborted, so the look is told to
    /// end after the tick it is in: nothing reads the free space or removes a unit once this returns.
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        let _ = (&mut self.handle).await;
    }
}

impl Drop for PruneTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Starts the look: every `settings.every` it runs [`tick`] on `target` and hands a prune that removed something to `report`.
pub fn start(
    settings: PruneSettings,
    target: PathBuf,
    free: Arc<dyn FreeProbe>,
    report: Report,
) -> PruneTask {
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(settings.every) => {}
                _ = stopped.changed() => break,
            }
            if *stopped.borrow() {
                break;
            }
            if let Some((pruned, free_after)) =
                tick(&settings, &target, free.clone(), SystemTime::now()).await
            {
                report(&pruned, free_after);
            }
        }
    });
    PruneTask { handle, stop }
}
