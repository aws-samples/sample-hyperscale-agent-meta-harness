//! Ctrl-G, the draft in the user's editor.
//!
//! * **The editor.** `$VISUAL` if it is not blank, else `$EDITOR` ([`Editor::from_env_values`]). The value is split like a shell line by
//!   [`crate::router::split_words`] (single and double quotes, backslash escapes; nothing is expanded) into an argument vector: the
//!   program, its fixed arguments, and the draft file last. It is started with [`std::process::Command`] and no shell, so `$(...)`, `;`,
//!   `&&` and the like are only characters. With neither variable set there is no editor ([`EditError::NoEditor`]); this module does not
//!   guess `vi`.
//! * **The file.** The draft goes to `draft.md` in a directory of its own, `ham-draft-<pid>-<time>-<n>`, made (mode 0700) under the
//!   user's temp directory ([`default_dir`], which follows `TMPDIR`, made absolute); the file is created exclusively with mode 0600 and
//!   a final line break. Nobody else can enter the directory, an editor's swap, backup or autosave file lands inside it, and the whole
//!   directory is removed on every path that unwinds: after the edit, on an error, on a panic. (A kill that cannot be caught leaves
//!   it, as it would leave any temp file.) The read-back refuses anything that is not a regular file.
//! * **The run.** The child runs through a closure, so the TUI hands it the terminal (`suspend_for`) and a test hands it a stand-in. A
//!   non-zero exit (`:cq` in vim) or a removed file cancels and the draft is left as it was. Text that is not UTF-8, or longer than
//!   [`MAX_DRAFT_BYTES`], is an error. [`edit_with_report`] also says how long the editor ran: an editor that returns at once with
//!   nothing changed is usually one that opens a window and returns (`code` without `--wait`), and [`Report::hint`] says so.
//! * **What comes back.** Line breaks as `\n`, escape sequences and controls removed (as for a paste), trailing line breaks cut. A text
//!   equal to the draft after the same cut is [`Outcome::Unchanged`].

use crate::router::split_words;
use crate::sanitize::sanitize;
use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The largest draft read back from the editor's file.
pub const MAX_DRAFT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// Neither `$VISUAL` nor `$EDITOR` is set.
    NoEditor,
    /// The editor command cannot be used (a quote is not closed, it holds a control character).
    BadCommand(String),
    /// The editor did not start.
    Spawn {
        program: String,
        reason: String,
    },
    /// The file could not be made, written or read.
    Io(String),
    /// The file is longer than [`MAX_DRAFT_BYTES`].
    TooLarge,
    NotUtf8,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoEditor => f.write_str("no editor: set $VISUAL or $EDITOR"),
            EditError::BadCommand(why) => write!(f, "the editor command cannot be used: {why}"),
            EditError::Spawn { program, reason } => {
                write!(f, "cannot start the editor `{program}`: {reason}")
            }
            EditError::Io(why) => f.write_str(why),
            EditError::TooLarge => write!(
                f,
                "the draft is larger than {} MiB",
                MAX_DRAFT_BYTES / (1024 * 1024)
            ),
            EditError::NotUtf8 => f.write_str(
                "the editor saved text that is not UTF-8; save it as UTF-8 and try again (the draft in the box is unchanged)",
            ),
        }
    }
}

impl std::error::Error for EditError {}

/// An editor command: the program and its fixed arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    argv: Vec<String>,
}

impl Editor {
    /// An editor from a command line such as `code --wait` or `"/Applications/My Editor/ed" -w`.
    pub fn from_command(line: &str) -> Result<Editor, EditError> {
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(EditError::BadCommand(
                "it holds a control character".to_string(),
            ));
        }
        let argv = split_words(line)
            .map_err(|_| EditError::BadCommand("a quote is not closed".to_string()))?;
        if argv.is_empty() {
            return Err(EditError::NoEditor);
        }
        Ok(Editor { argv })
    }

    /// `$VISUAL` if it is not blank, else `$EDITOR`. A damaged value is an error, not a reason to try the other.
    pub fn from_env_values(
        visual: Option<&str>,
        editor: Option<&str>,
    ) -> Result<Editor, EditError> {
        match [visual, editor]
            .into_iter()
            .flatten()
            .find(|v| !v.trim().is_empty())
        {
            Some(v) => Editor::from_command(v),
            None => Err(EditError::NoEditor),
        }
    }

    /// The editor of the environment of this process.
    pub fn from_env() -> Result<Editor, EditError> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        Editor::from_env_values(visual.as_deref(), editor.as_deref())
    }

    /// The program and its fixed arguments.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The command to run for `file`: the program, its arguments, then the file. No shell is involved.
    pub fn command_for(&self, file: &Path) -> Command {
        let mut c = Command::new(&self.argv[0]);
        c.args(&self.argv[1..]);
        c.arg(file);
        c
    }
}

/// What the edit came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The text is what it was (line breaks at the end aside).
    Unchanged,
    /// The editor returned another text.
    Edited { text: String, tainted: bool },
    /// The editor exited with an error or removed the file: the draft is left as it was.
    Cancelled,
}

/// The directory of the draft file: the user's temp directory (`TMPDIR`).
pub fn default_dir() -> PathBuf {
    // nosemgrep: temp-dir -- the draft is never written in the shared directory itself: each edit makes a private 0700 directory under it
    std::env::temp_dir()
}

/// The private directory of one edit; removing it whole is the last thing that happens to it.
struct DraftDir {
    path: PathBuf,
}

impl Drop for DraftDir {
    fn drop(&mut self) {
        // The editor's own files (swap, backup, autosave) sit beside the draft and go with the directory. A failure to remove it cannot
        // be reported from here.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Makes the private directory under `parent` (mode 0700, a name nobody can predict is not needed: `mkdir` fails if it exists) and
/// the draft file in it (mode 0600, exclusive).
fn create_private(parent: &Path) -> Result<(File, DraftDir, PathBuf), EditError> {
    static N: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!(
            "ham-draft-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let dir = parent.join(name);
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(EditError::Io(format!(
                    "cannot make the draft directory in {}: {e}",
                    parent.display()
                )));
            }
        }
        let guard = DraftDir { path: dir.clone() };
        let path = dir.join("draft.md");
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).map_err(|e| {
            EditError::Io(format!(
                "cannot make the draft file in {}: {e}",
                dir.display()
            ))
        })?;
        return Ok((file, guard, path));
    }
    Err(EditError::Io(format!(
        "cannot find a free name for the draft directory in {}",
        parent.display()
    )))
}

fn tidy(text: &str) -> &str {
    text.trim_end_matches('\n')
}

/// How long the editor ran and what came of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    /// From the start of the editor to its exit.
    pub elapsed: Duration,
}

/// An editor that returns sooner than this with nothing changed is suspected of opening a window and returning.
pub const QUICK_RETURN: Duration = Duration::from_secs(1);

impl Report {
    /// The editor came back at once and changed nothing.
    pub fn returned_at_once(&self) -> bool {
        self.outcome == Outcome::Unchanged && self.elapsed < QUICK_RETURN
    }

    /// What to tell the user when the editor returned at once with nothing changed.
    pub fn hint(&self) -> Option<&'static str> {
        self.returned_at_once().then_some(
            "The editor returned at once and changed nothing. An editor that opens a window and returns needs its wait flag (for example `code --wait`).",
        )
    }
}

/// Edits `draft` with `editor`: `run` starts the child (it gets the command with the file as its last argument) and says how it ended.
pub fn edit_with<F>(editor: &Editor, draft: &str, dir: &Path, run: F) -> Result<Outcome, EditError>
where
    F: FnOnce(&mut Command) -> std::io::Result<ExitStatus>,
{
    edit_with_report(editor, draft, dir, run).map(|r| r.outcome)
}

/// [`edit_with`] with the time the editor ran.
pub fn edit_with_report<F>(
    editor: &Editor,
    draft: &str,
    dir: &Path,
    run: F,
) -> Result<Report, EditError>
where
    F: FnOnce(&mut Command) -> std::io::Result<ExitStatus>,
{
    // An absolute path, so a relative TMPDIR cannot give the editor a name that reads as an option or points somewhere else.
    let parent = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let (mut file, _guard, path) = create_private(&parent)?;
    let io = |what: &str, e: std::io::Error| EditError::Io(format!("{what}: {e}"));
    file.write_all(draft.as_bytes())
        .map_err(|e| io("cannot write the draft file", e))?;
    if !draft.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|e| io("cannot write the draft file", e))?;
    }
    file.flush()
        .map_err(|e| io("cannot write the draft file", e))?;
    drop(file);

    let mut command = editor.command_for(&path);
    let began = Instant::now();
    let status = run(&mut command).map_err(|e| EditError::Spawn {
        program: editor.argv[0].clone(),
        reason: e.to_string(),
    })?;
    let elapsed = began.elapsed();
    let done = |outcome| Ok(Report { outcome, elapsed });
    if !status.success() {
        return done(Outcome::Cancelled);
    }

    // Whatever the editor did to the file (a rename over it, a link), only a regular file is read.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => {
            return Err(EditError::Io(
                "the editor left something other than a regular file where the draft was"
                    .to_string(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return done(Outcome::Cancelled),
        Err(e) => return Err(io("cannot read the draft file", e)),
    }
    let mut bytes = Vec::new();
    match File::open(&path) {
        Ok(f) => {
            f.take(MAX_DRAFT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| io("cannot read the draft file", e))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return done(Outcome::Cancelled),
        Err(e) => return Err(io("cannot read the draft file", e)),
    }
    if bytes.len() > MAX_DRAFT_BYTES {
        return Err(EditError::TooLarge);
    }
    let raw = String::from_utf8(bytes).map_err(|_| EditError::NotUtf8)?;
    let text = sanitize(&raw.replace("\r\n", "\n").replace('\r', "\n"));
    if tidy(&text) == tidy(draft) {
        return done(Outcome::Unchanged);
    }
    done(Outcome::Edited {
        text: tidy(&text).to_string(),
        tainted: true,
    })
}

/// [`edit_with`] with the child started the plain way, inheriting the terminal.
pub fn edit(editor: &Editor, draft: &str, dir: &Path) -> Result<Outcome, EditError> {
    edit_with(editor, draft, dir, |c| c.status())
}
