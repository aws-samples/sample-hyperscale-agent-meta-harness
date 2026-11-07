//! Where the lifecycle pack's records of a run are.
//! COMPAT-CAH-DATA: a repository that keeps them in the earlier folder `.cah` is worked in that folder until VERSION 6.00 (see "One folder per run").
//!
//! The pack has the model write a goal card, a run log, red records, research notes and a context index into the repository. They sit in
//! `.ham/lifecycle`, `.ham/research` and `.ham/context` unless the repository says otherwise.
//! The
//! repository says otherwise in the file [`LOCATOR`], `.ham/records`. Its first line (blank lines and lines that start with `#` are skipped) is
//!
//! - `off`: the run writes none of these records, and nothing here reads one;
//! - a folder, relative to the repository, that takes the place of `.ham`: the records sit in `<folder>/lifecycle`, `<folder>/research` and
//!   `<folder>/context`;
//! - anything else is [`Records::Invalid`]: the pack ends the run with `BLOCKED:` and the places stay the default ones here.
//!
//! The places are never searched in two folders: a log left in `docs/lifecycle` by an older run is not read unless `.ham/records` says `docs`.
//!
//! One folder per run (COMPAT-CAH-DATA, ends at VERSION 6.00). The folder that stands for `.ham` is chosen from the repository once, when the run
//! starts: a folder holds state when it has a regular file `records` or `test-command` in it, or a regular file at any depth under `lifecycle/`,
//! `research/` or `context/` (a link counts for nothing, and so does an empty folder). State only in `.cah`: the run uses `.cah`, and the locator is `.cah/records`. State only
//! in `.ham`, or in neither: `.ham`. State in both: [`Records::Conflict`], and the run does not start. An empty `.ham` never hides a populated `.cah`.
//!
//! One reader serves the clean-checkout report (`report/plan.rs`), the observation of the run log (`observe.rs`) and the progress fingerprint of
//! the goal loop (`goal/tree.rs`), so they agree with the pack and with its self-check script (`lifecycle_check.py`, which has the same grammar;
//! `fixtures/lifecycle/records-locator.json` holds the cases both readers must give the same value). The file is the user's: it is read without
//! following a link, from a folder that is no link, and it holds one short value. It is never written here.

use crate::direct::snapshot::read_regular_nofollow;
use std::path::Path;

/// The records folder of the earlier name (COMPAT-CAH-DATA): a repository that holds state only here is worked in this folder.
pub const EARLIER_BASE: &str = ".cah";
/// The project's own command file in each folder, in the order they are read: `.ham/test-command`, else `.cah/test-command` (COMPAT-CAH-DATA).
pub const TEST_COMMAND_FILES: [&str; 2] = [".ham/test-command", ".cah/test-command"];
/// The files that make a records folder hold state when they sit directly in it.
const STATE_FILES: [&str; 2] = ["records", "test-command"];
/// The folders that make a records folder hold state when a regular file sits at any depth below them.
const STATE_DIRS: [&str; 3] = ["lifecycle", "research", "context"];

/// The file in the repository that says where the records are.
pub const LOCATOR: &str = ".ham/records";
/// The folder that holds `lifecycle/`, `research/` and `context/` when no locator names another.
pub const DEFAULT_BASE: &str = ".ham";
/// The most bytes the locator may hold: a value and a few lines of comment.
const MAX_LOCATOR_BYTES: usize = 4096;
/// The longest folder, in bytes.
const MAX_DIR_BYTES: usize = 100;

/// What `.ham/records` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Records {
    /// No locator (or an empty one): `.ham/lifecycle`, `.ham/research` and `.ham/context`.
    Default,
    /// The records are in `<folder>/lifecycle`, `<folder>/research` and `<folder>/context`; the folder is relative to the repository, without a
    /// trailing slash.
    Moved(String),
    /// The repository turned the records off.
    Off,
    /// The locator holds something that is neither `off` nor a plain relative folder; the text says what is wrong. The places read are the
    /// default ones.
    Invalid(String),
    /// Both `.ham` and `.cah` hold state (COMPAT-CAH-DATA); the text names both folders. A run in such a repository does not start. The places read are
    /// the default ones.
    Conflict(String),
}

fn too_long() -> String {
    format!("the file is longer than {MAX_LOCATOR_BYTES} bytes")
}

/// The folder a locator value names, or why it is not one. Letters, digits, `.`, `_`, `-` and `/` only, no part empty, `.` or `..`, none
/// starting with `-` (a tool could read it as an option) and none `.git`.
fn folder(value: &str) -> Result<String, String> {
    let dir = value.trim_end_matches('/');
    if dir.is_empty() {
        return Err("the folder is empty".into());
    }
    if dir.len() > MAX_DIR_BYTES {
        return Err(format!("the folder is longer than {MAX_DIR_BYTES} bytes"));
    }
    if dir.starts_with('/') {
        return Err("the folder must be relative to the repository, not absolute".into());
    }
    if !dir
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return Err("the folder may hold letters, digits, '.', '_', '-' and '/' only".into());
    }
    for part in dir.split('/') {
        if part.is_empty() {
            return Err("the folder has an empty part".into());
        }
        if part == "." || part == ".." {
            return Err("the folder must not hold '.' or '..' as a part".into());
        }
        if part.starts_with('-') {
            return Err("no part of the folder may start with '-'".into());
        }
        if part.eq_ignore_ascii_case(".git") {
            return Err("the folder must not be inside .git".into());
        }
    }
    Ok(dir.to_string())
}

impl Records {
    /// The value of a locator's text. Only a space, a tab and a carriage return are trimmed from a line, as the self-check script and the contract read it (`str::trim` would
    /// also strip U+00A0, U+0085, U+2028, U+3000 and the other Unicode white space, and name a place the script calls invalid).
    pub fn parse(text: &str) -> Records {
        let Some(first) = text
            .lines()
            .map(|l| l.trim_matches([' ', '\t', '\r']))
            .find(|l| !l.is_empty() && !l.starts_with('#'))
        else {
            return Records::Default;
        };
        if first.eq_ignore_ascii_case("off") {
            return Records::Off;
        }
        match folder(first) {
            Ok(dir) => Records::Moved(dir),
            Err(why) => Records::Invalid(why),
        }
    }

    /// What the repository at `tree` says: the folder that stands for `.ham` is chosen first ([`Records::Conflict`] when both `.ham` and `.cah` hold state;
    /// `.cah` when only it does; else `.ham`); then its locator file is read. [`Records::Default`] when `.ham` is the folder and the locator is missing, is a
    /// link or is no regular file; [`Records::Moved`]`(".cah")` when `.cah` is the folder and says nothing else; [`Records::Invalid`] when the locator is
    /// longer than a short value needs (the file reader gives up on a regular file over 1 MiB, which is no reason to read the default places) or is no UTF-8
    /// text.
    pub fn read(tree: &Path) -> Records {
        match choose(tree) {
            Err(why) => Records::Conflict(why),
            Ok(folder) => Self::read_in(tree, folder),
        }
    }

    /// [`Records::read`] for the folder a run chose when it started ([`choose`]): the locator of that folder is read now, because the engine may write
    /// it during the run, but the folder is not chosen again.
    pub fn read_in(tree: &Path, folder: &str) -> Records {
        match Self::read_locator(tree, folder) {
            Records::Default if folder == EARLIER_BASE => Records::Moved(EARLIER_BASE.to_string()),
            other => other,
        }
    }

    /// The locator `<folder>/records` of the work tree `tree`.
    fn read_locator(tree: &Path, folder: &str) -> Records {
        match std::fs::symlink_metadata(tree.join(folder)) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => return Records::Default,
        }
        let locator = tree.join(folder).join("records");
        let Some(bytes) = read_regular_nofollow(&locator) else {
            return match std::fs::symlink_metadata(&locator) {
                Ok(m) if m.file_type().is_file() && m.len() > MAX_LOCATOR_BYTES as u64 => {
                    Records::Invalid(too_long())
                }
                _ => Records::Default,
            };
        };
        if bytes.len() > MAX_LOCATOR_BYTES {
            return Records::Invalid(too_long());
        }
        match String::from_utf8(bytes) {
            Ok(text) => Records::parse(&text),
            Err(_) => Records::Invalid("the file is not UTF-8 text".into()),
        }
    }

    /// Why the run cannot start in this repository: both records folders hold state ([`Records::Conflict`]); `None` otherwise.
    pub fn conflict(&self) -> Option<&str> {
        match self {
            Records::Conflict(why) => Some(why),
            _ => None,
        }
    }

    /// Why the locator cannot be used, for a log line; `None` for a usable value.
    pub fn problem(&self) -> Option<&str> {
        match self {
            Records::Invalid(why) | Records::Conflict(why) => Some(why),
            _ => None,
        }
    }

    /// The folder that holds `lifecycle/`, `research/` and `context/`; `None` when the records are off.
    pub fn base(&self) -> Option<&str> {
        match self {
            Records::Default | Records::Invalid(_) | Records::Conflict(_) => Some(DEFAULT_BASE),
            Records::Moved(dir) => Some(dir),
            Records::Off => None,
        }
    }

    /// The folder of the goal card, the run log and the red records; `None` when the records are off.
    pub fn dir(&self) -> Option<String> {
        self.base().map(|b| format!("{b}/lifecycle"))
    }

    /// The goal card, relative to the repository.
    pub fn goal(&self) -> Option<String> {
        self.dir().map(|d| format!("{d}/GOAL.md"))
    }

    /// The run log, relative to the repository.
    pub fn log(&self) -> Option<String> {
        self.dir().map(|d| format!("{d}/LOG.md"))
    }

    /// The folder of the red records.
    pub fn red_dir(&self) -> Option<String> {
        self.dir().map(|d| format!("{d}/red"))
    }

    /// The folder of the research notes.
    pub fn research_dir(&self) -> Option<String> {
        self.base().map(|b| format!("{b}/research"))
    }

    /// The context index.
    pub fn context_index(&self) -> Option<String> {
        self.base().map(|b| format!("{b}/context/INDEX.md"))
    }

    /// What the progress fingerprint of the goal loop leaves out: the folder of the card, the log and the red records, which change every round
    /// (the research notes and the context index are work, as they are in `.ham/research` and `.ham/context`). Nothing when the records are off.
    /// `goal/tree.rs` leaves out `.ham/lifecycle` and `docs/lifecycle` always, so a run under an older rules file is not taken for progress.
    pub fn progress_excludes(&self) -> Vec<String> {
        self.dir().into_iter().collect()
    }
}

/// The folder that stands for `.ham` in the repository at `tree` (COMPAT-CAH-DATA): `.cah` when only it holds state, else `.ham`; the words of the
/// refusal when both hold state. A run chooses once, when it starts, and keeps the answer.
pub fn choose(tree: &Path) -> Result<&'static str, String> {
    match (
        holds_state(tree, DEFAULT_BASE),
        holds_state(tree, EARLIER_BASE),
    ) {
        (true, true) => Err(format!(
            "this repository keeps records in both {DEFAULT_BASE} and {EARLIER_BASE}, and a run uses one folder; \
             move what {EARLIER_BASE} holds into {DEFAULT_BASE} (or remove the one you do not use) and start the run again"
        )),
        (false, true) => Ok(EARLIER_BASE),
        _ => Ok(DEFAULT_BASE),
    }
}

/// The first line of the first prompt when the run works in the earlier folder: it names the folder, so the engine and the harness agree. `None` for
/// `.ham`, where the user's text goes as typed.
pub fn prompt_line(folder: &str) -> Option<String> {
    (folder == EARLIER_BASE).then(|| {
        format!(
            "Records folder of this run: {folder} (this repository keeps its lifecycle records there; where the rules, skills and tools say {DEFAULT_BASE}, use {folder})."
        )
    })
}

/// Whether the folder `name` below `tree` holds state (COMPAT-CAH-DATA): it is a real folder, no link, and it holds a regular file named by
/// [`STATE_FILES`] directly in it, or a regular file at any depth under a folder named by [`STATE_DIRS`]. A link counts for nothing, and so does an
/// empty folder. `fixtures/lifecycle/records-locator.json` (`baseCases`) holds the cases the self-check script of the lifecycle pack and this reader
/// must answer alike.
fn holds_state(tree: &Path, name: &str) -> bool {
    let dir = tree.join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_dir() => {}
        _ => return false,
    }
    if STATE_FILES
        .iter()
        .any(|f| std::fs::symlink_metadata(dir.join(f)).is_ok_and(|m| m.file_type().is_file()))
    {
        return true;
    }
    let mut budget = SCAN_ENTRIES;
    STATE_DIRS
        .iter()
        .any(|d| has_file_below(&dir.join(d), &mut budget, SCAN_DEPTH))
}

/// How many entries and how deep one look for a file goes before it gives up (a tree of that many folders and no file is no state).
const SCAN_ENTRIES: u32 = 5000;
const SCAN_DEPTH: u32 = 16;

/// Whether the real folder `path` holds a regular file at any depth (a link is followed nowhere).
fn has_file_below(path: &Path, budget: &mut u32, depth: u32) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_dir() || depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() || (kind.is_dir() && has_file_below(&entry.path(), budget, depth - 1)) {
            return true;
        }
    }
    false
}
