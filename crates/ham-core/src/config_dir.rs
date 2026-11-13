// COMPAT-CAH: the folder of the earlier command name, `~/.config/cah`, is read (never written) until VERSION 5.50. At that
// version this file goes: `config::config_path` returns the folder `ham` again and `ham config migrate` is removed with its words.
//! Where the files of the client are.
//!
//! The client keeps its files in one folder, `$XDG_CONFIG_HOME/ham` or `~/.config/ham`: `config.toml`, `engines/`, `rules/`, `skills/`,
//! `continue.md` and `review.md` ([`USER_ENTRIES`]). Before the rename the folder was `cah`. For the window:
//!
//! - the folder `ham` is used when it holds user content: one of the six entries, a file, or a folder with something in it. An empty folder
//!   does not count, so a folder made by hand or by an interrupted copy never hides the files of the earlier one;
//! - otherwise, when the folder `cah` holds user content, it is read in its place, **read-only** ([`State::EarlierOnly`]): a command that would write
//!   there is refused ([`write_refusal`]) and nothing ever writes it;
//! - when both hold content the folder `ham` wins as a whole, never file by file, and `ham config show` says the other one is not read
//!   ([`Dirs::earlier_unread`]);
//! - [`plan_migrate`] and [`apply_migrate`] are `ham config migrate`: the six entries are copied (never `setup/`) byte for byte with their
//!   modes. A `ham` folder that does not exist is made whole in a temporary sibling folder and renamed into place in one step. A `ham` folder
//!   that exists (`setup.sh` makes `ham/setup/` first) is filled in place between a marker file ([`MIGRATING`]) and its removal: while the
//!   marker is there the folder does not count as holding content, so a copy that stops half way never hides the `cah` files, and the next
//!   run finishes it. A file that is already in `ham` and differs is never overwritten, not even one that appears while the copy runs; a symbolic
//!   link, a name that is not text and a file that cannot be read refuse the copy before anything is written; the earlier folder is not changed.
//!
//! One function ([`dirs`]) decides, from the folders as they are at that moment, for the whole folder; `config::config_path` is its
//! `config.toml`, and the rules, skills and engine paths derive from that.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The folder of the program.
pub const CURRENT_NAME: &str = "ham";
/// The folder of the earlier command name.
pub const EARLIER_NAME: &str = "cah";

/// The entries of the folder that are the user's own files: a folder holds user content when it holds one of these. `setup/` (receipts, logs
/// and downloads of the setup script) is not one of them.
pub const USER_ENTRIES: [&str; 6] = [
    "config.toml",
    "engines",
    "rules",
    "skills",
    "continue.md",
    "review.md",
];

/// The marker file `ham config migrate` keeps in the folder `ham` while it fills a folder that already exists. A `ham` folder with the marker does
/// not count as holding content while the earlier folder does, so a copy that stopped half way never hides the earlier files.
pub const MIGRATING: &str = ".ham-migrating";

/// Which folder holds the files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The folder `ham` holds user content.
    Current,
    /// Neither folder holds user content: files are made in the folder `ham`.
    Fresh,
    /// Only the folder `cah` holds user content: it is read, and nothing is written.
    EarlierOnly,
}

/// The two folders and the decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    pub current: PathBuf,
    pub earlier: PathBuf,
    pub state: State,
    /// Both folders hold content, so the folder `cah` is not read.
    pub earlier_unread: bool,
}

impl Dirs {
    /// The folder the client reads and writes: `cah` only in [`State::EarlierOnly`].
    pub fn in_use(&self) -> &Path {
        if self.state == State::EarlierOnly {
            &self.earlier
        } else {
            &self.current
        }
    }
}

fn base(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(x) = env("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(x));
    }
    env("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".config"))
}

/// A file a file browser leaves in a folder: it is not the user's.
fn is_noise(name: &std::ffi::OsStr) -> bool {
    name == ".DS_Store"
}

/// Whether `dir` holds user content: one of [`USER_ENTRIES`] is a file, or a folder with something in it (a file browser's own file does not
/// count). A link is followed. What cannot be looked at (no permission, an error) is taken to hold something, never for empty.
pub fn holds_user_content(dir: &Path) -> bool {
    USER_ENTRIES.iter().any(|name| {
        let path = dir.join(name);
        match fs::metadata(&path) {
            Ok(m) if m.is_dir() => match fs::read_dir(&path) {
                Ok(mut entries) => entries.any(|e| e.map_or(true, |e| !is_noise(&e.file_name()))),
                Err(_) => true,
            },
            Ok(m) => m.is_file(),
            Err(e) => !matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory),
        }
    })
}

/// The two folders and which one is in use. `None` when the environment names no place (`XDG_CONFIG_HOME` and `HOME` both unset or empty).
pub fn dirs(env: &dyn Fn(&str) -> Option<String>) -> Option<Dirs> {
    let base = base(env)?;
    let (current, earlier) = (base.join(CURRENT_NAME), base.join(EARLIER_NAME));
    let has_earlier = holds_user_content(&earlier);
    // A copy into this folder that stopped half way: while the earlier folder is there, the folder does not win.
    let unfinished = has_earlier && fs::symlink_metadata(current.join(MIGRATING)).is_ok();
    let has_current = !unfinished && holds_user_content(&current);
    let state = if has_current {
        State::Current
    } else if has_earlier {
        State::EarlierOnly
    } else {
        State::Fresh
    };
    Some(Dirs {
        current,
        earlier,
        state,
        earlier_unread: has_current && has_earlier,
    })
}

/// The text of the refusal of a write while only the earlier folder holds the files.
pub fn refusal_text(d: &Dirs) -> String {
    format!(
        "Your settings are read from {}, the folder of the old name, and ham writes nothing there. Copy them to {} with `ham config migrate`, then repeat this command.",
        d.earlier.display(),
        d.current.display()
    )
}

/// The refusal of a write: `Some(text)` while only the earlier folder holds the files, else `None`. The commands that write the user's files
/// (`config set|unset|edit`, `engine ... init|edit|set|unset|import`, `rules init|edit`) ask before they open an editor or build anything.
pub fn write_refusal(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    dirs(env)
        .filter(|d| d.state == State::EarlierOnly)
        .map(|d| refusal_text(&d))
}

/// The notice made while only the earlier folder holds the files (printed once, before the screen opens, unless the window's switch is on).
pub fn read_notice(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    dirs(env).filter(|d| d.state == State::EarlierOnly).map(|d| {
        format!(
            "Your settings are read from {}, the folder of the old name. Copy them with: ham config migrate",
            d.earlier.display()
        )
    })
}

// ---- ham config migrate ----------------------------------------------------------------------------------------------------------

/// One thing to create in the new folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The path below the folder, with `/`.
    pub rel: String,
    pub is_dir: bool,
    /// Bytes of a file (0 for a folder).
    pub bytes: u64,
    /// The permission bits of the source.
    pub mode: u32,
}

/// What `ham config migrate` would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Folders and files that are not in the new folder yet, parents first.
    pub copy: Vec<Item>,
    /// Files that are in the new folder already, byte for byte.
    pub same: Vec<String>,
    /// The new folder holds no user content: it is made whole in a temporary sibling folder and renamed into place when it does not exist (or is
    /// empty), and filled in place between the marker [`MIGRATING`] and its removal when it exists with other things in it (`setup/`).
    pub whole_folder: bool,
}

/// Why there is nothing to do or the copy is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The environment names no config location.
    NoLocation,
    /// The earlier folder holds no user content.
    NothingToMigrate {
        from: PathBuf,
    },
    /// Symbolic links in the earlier folder or in the new one, or the folder itself being one.
    Symlink(Vec<String>),
    /// Entries that are neither a file nor a folder, or whose names are not text.
    NotRegular(Vec<String>),
    /// Files that are in the new folder with other content; none is overwritten.
    Differs(Vec<String>),
    Io(String),
}

fn io_text(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}

fn io(path: &Path, e: &std::io::Error) -> Refusal {
    Refusal::Io(io_text(path, e))
}

/// The bytes of a regular file that is not a link: the file looked at before it is opened is the file that is read (same device and inode), so a link
/// put in its place in between is not followed.
fn read_regular(path: &Path) -> Result<Vec<u8>, Refusal> {
    let before = fs::symlink_metadata(path).map_err(|e| io(path, &e))?;
    if before.file_type().is_symlink() {
        return Err(Refusal::Symlink(vec![path.display().to_string()]));
    }
    if !before.file_type().is_file() {
        return Err(Refusal::NotRegular(vec![path.display().to_string()]));
    }
    let mut file = fs::File::open(path).map_err(|e| io(path, &e))?;
    let after = file.metadata().map_err(|e| io(path, &e))?;
    if !after.is_file() || (after.dev(), after.ino()) != (before.dev(), before.ino()) {
        return Err(Refusal::Io(format!(
            "{}: changed while it was being read",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| io(path, &e))?;
    Ok(bytes)
}

fn walk(
    base: &Path,
    rel: &str,
    items: &mut Vec<Item>,
    links: &mut Vec<String>,
    odd: &mut Vec<String>,
) -> Result<(), Refusal> {
    let path = base.join(rel);
    let meta = fs::symlink_metadata(&path).map_err(|e| io(&path, &e))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        links.push(rel.to_string());
    } else if ft.is_dir() {
        items.push(Item {
            rel: rel.to_string(),
            is_dir: true,
            bytes: 0,
            mode: meta.mode() & 0o7777,
        });
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(&path).map_err(|e| io(&path, &e))? {
            let entry = entry.map_err(|e| io(&path, &e))?;
            match entry.file_name().into_string() {
                Ok(name) => names.push(name),
                Err(raw) => odd.push(format!("{rel}/{}", raw.to_string_lossy())),
            }
        }
        names.sort();
        for name in names {
            walk(base, &format!("{rel}/{name}"), items, links, odd)?;
        }
    } else if ft.is_file() {
        items.push(Item {
            rel: rel.to_string(),
            is_dir: false,
            bytes: meta.len(),
            mode: meta.mode() & 0o7777,
        });
    } else {
        odd.push(rel.to_string());
    }
    Ok(())
}

/// What `ham config migrate` would do in this environment. Nothing is written.
pub fn plan_migrate(env: &dyn Fn(&str) -> Option<String>) -> Result<Plan, Refusal> {
    let d = dirs(env).ok_or(Refusal::NoLocation)?;
    let (from, to) = (d.earlier.clone(), d.current.clone());
    if !holds_user_content(&from) {
        return Err(Refusal::NothingToMigrate { from });
    }
    for folder in [&from, &to] {
        if fs::symlink_metadata(folder).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Refusal::Symlink(vec![folder.display().to_string()]));
        }
    }
    let (mut items, mut links, mut odd) = (Vec::new(), Vec::new(), Vec::new());
    for entry in USER_ENTRIES {
        match fs::symlink_metadata(from.join(entry)) {
            Ok(_) => walk(&from, entry, &mut items, &mut links, &mut odd)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io(&from.join(entry), &e)),
        }
    }
    if !links.is_empty() {
        links.sort();
        return Err(Refusal::Symlink(links));
    }
    if !odd.is_empty() {
        odd.sort();
        return Err(Refusal::NotRegular(odd));
    }
    // A file that cannot be read is found here, as the copy would find it, so that a dry run says the same.
    for item in items.iter().filter(|i| !i.is_dir) {
        let path = from.join(&item.rel);
        fs::File::open(&path).map_err(|e| io(&path, &e))?;
    }
    let whole_folder = !holds_user_content(&to);
    if whole_folder {
        return Ok(Plan {
            from,
            to,
            copy: items,
            same: Vec::new(),
            whole_folder,
        });
    }
    let (mut copy, mut same, mut differs, mut linked) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for item in items {
        let target = to.join(&item.rel);
        match fs::symlink_metadata(&target) {
            Err(e) if e.kind() == ErrorKind::NotFound => copy.push(item),
            Err(e) => return Err(io(&target, &e)),
            Ok(m) if m.file_type().is_symlink() => linked.push(item.rel),
            Ok(m) if item.is_dir => {
                if !m.is_dir() {
                    differs.push(item.rel);
                }
            }
            Ok(m) if m.is_file() => {
                if read_regular(&from.join(&item.rel))? == read_regular(&target)? {
                    same.push(item.rel);
                } else {
                    differs.push(item.rel);
                }
            }
            Ok(_) => differs.push(item.rel),
        }
    }
    if !linked.is_empty() {
        linked.sort();
        return Err(Refusal::Symlink(linked));
    }
    if !differs.is_empty() {
        differs.sort();
        return Err(Refusal::Differs(differs));
    }
    Ok(Plan {
        from,
        to,
        copy,
        same,
        whole_folder,
    })
}

/// A temporary folder that goes with its contents unless it was placed.
struct Scratch {
    path: PathBuf,
    armed: bool,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// A name no other process, run or file of the user's has: the temporary files and folders of a copy.
fn unique() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

/// Makes the directory data durable: best effort, a filesystem that cannot do it is not an error.
fn sync_dir(path: &Path) {
    if let Ok(dir) = fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Makes `dst` a link to `part`'s inode only when nothing is at `dst`; a file that is there with the same bytes is fine, one that differs is
/// never replaced.
fn publish(part: &Path, dst: &Path, rel: &str, bytes: &[u8]) -> Result<(), Refusal> {
    let same_there = || read_regular(dst).is_ok_and(|b| b == bytes);
    match fs::hard_link(part, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            if same_there() {
                Ok(())
            } else {
                Err(Refusal::Differs(vec![rel.to_string()]))
            }
        }
        // A filesystem without hard links: rename, only when nothing is there.
        Err(_) => match fs::symlink_metadata(dst) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::rename(part, dst).map_err(|e| io(dst, &e))
            }
            Err(e) => Err(io(dst, &e)),
            Ok(_) if same_there() => Ok(()),
            Ok(_) => Err(Refusal::Differs(vec![rel.to_string()])),
        },
    }
}

/// Copies one file: written beside its place under a private mode and a name of its own, synced, read back and compared with the source, given
/// the source's mode, then published without replacing anything. The temporary file is removed when this call made it, whatever happened.
fn copy_file(src: &Path, dst: &Path, rel: &str, mode: u32) -> Result<(), Refusal> {
    let bytes = read_regular(src)?;
    let dir = dst
        .parent()
        .ok_or_else(|| Refusal::Io(format!("{}: no folder", dst.display())))?;
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let part = dir.join(format!(".{name}.ham-migrate-{}", unique()));
    let mut made = false;
    let done = (|| -> Result<(), Refusal> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&part)
            .map_err(|e| io(&part, &e))?;
        made = true;
        file.write_all(&bytes).map_err(|e| io(&part, &e))?;
        file.sync_all().map_err(|e| io(&part, &e))?;
        drop(file);
        if fs::read(&part).map_err(|e| io(&part, &e))? != bytes {
            return Err(Refusal::Io(format!(
                "{}: the copy differs from the original",
                dst.display()
            )));
        }
        fs::set_permissions(&part, fs::Permissions::from_mode(mode)).map_err(|e| io(&part, &e))?;
        publish(&part, dst, rel, &bytes)
    })();
    if made {
        let _ = fs::remove_file(&part);
    }
    done
}

/// Makes the folder `target` private to its owner, for the copy to fill; the mode of the source is set when all is in place. A folder that is
/// already there is left as it is when it is a folder, and refused when it is anything else.
fn make_dir(target: &Path, rel: &str) -> Result<bool, Refusal> {
    match fs::DirBuilder::new().mode(0o700).create(target) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => match fs::symlink_metadata(target) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(false),
            Ok(m) if m.file_type().is_symlink() => Err(Refusal::Symlink(vec![rel.to_string()])),
            Ok(_) => Err(Refusal::Differs(vec![rel.to_string()])),
            Err(e) => Err(io(target, &e)),
        },
        Err(e) => Err(io(target, &e)),
    }
}

/// Puts the items of the plan below `root`: folders private first and with the mode of their source at the end, files through [`copy_file`].
fn place_items(plan: &Plan, root: &Path) -> Result<(), Refusal> {
    let mut made: Vec<(PathBuf, u32)> = Vec::new();
    for item in &plan.copy {
        let target = root.join(&item.rel);
        if item.is_dir {
            if make_dir(&target, &item.rel)? {
                made.push((target, item.mode));
            }
        } else {
            copy_file(&plan.from.join(&item.rel), &target, &item.rel, item.mode)?;
        }
    }
    for (dir, mode) in made.into_iter().rev() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).map_err(|e| io(&dir, &e))?;
    }
    Ok(())
}

/// The finished copy renamed into place in one step; `Ok(false)` when the new folder is not empty (so another way is needed).
fn place_whole(plan: &Plan, parent: &Path) -> Result<bool, Refusal> {
    let tmp = parent.join(format!(".ham-migrate-{}", unique()));
    fs::create_dir(&tmp).map_err(|e| io(&tmp, &e))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)).map_err(|e| io(&tmp, &e))?;
    let mut scratch = Scratch {
        path: tmp.clone(),
        armed: true,
    };
    place_items(plan, &tmp)?;
    let mode = fs::metadata(&plan.from).map_or(0o700, |m| m.mode() & 0o777) | 0o700;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| io(&tmp, &e))?;
    match fs::rename(&tmp, &plan.to) {
        Ok(()) => {
            scratch.armed = false;
            sync_dir(parent);
            Ok(true)
        }
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(io(&plan.to, &e)),
    }
}

/// The copy in place: the marker, the items, then the removal of the marker, which is the one step that makes the folder count.
fn place_in_existing(plan: &Plan) -> Result<(), Refusal> {
    if fs::symlink_metadata(&plan.to).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Refusal::Symlink(vec![plan.to.display().to_string()]));
    }
    fs::create_dir_all(&plan.to).map_err(|e| io(&plan.to, &e))?;
    let marker = plan.to.join(MIGRATING);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&marker)
        .map_err(|e| io(&marker, &e))?;
    file.write_all(b"A copy of the files of the earlier folder is unfinished here. `ham config migrate` finishes it.\n")
        .and_then(|()| file.sync_all())
        .map_err(|e| io(&marker, &e))?;
    drop(file);
    place_items(plan, &plan.to)?;
    sync_dir(&plan.to);
    fs::remove_file(&marker).map_err(|e| io(&marker, &e))?;
    sync_dir(&plan.to);
    Ok(())
}

fn is_absent_or_empty(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == ErrorKind::NotFound,
    }
}

/// Does what [`plan_migrate`] planned. A new folder that does not exist (or is empty) gets the finished copy in one rename. One that exists with
/// other things in it is filled in place between the marker [`MIGRATING`] and its removal. Nothing is overwritten: a file that is there and
/// differs, even one made after the plan, is [`Refusal::Differs`]. The earlier folder is only read.
pub fn apply_migrate(plan: &Plan) -> Result<(), Refusal> {
    let parent = plan
        .to
        .parent()
        .ok_or_else(|| Refusal::Io(format!("{}: no folder above it", plan.to.display())))?;
    fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
    if plan.whole_folder && is_absent_or_empty(&plan.to) && place_whole(plan, parent)? {
        return Ok(());
    }
    place_in_existing(plan)
}
