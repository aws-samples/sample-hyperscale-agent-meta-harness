//! `ham clone <runId> [dir]`: the delivered zip's `repo.bundle` becomes a local git
//! repository, with no git host and no git credential.
//!
//! Every delivered zip (complete, or the partial zip of a blocked run) holds `<top>/repo.bundle`, the full history of
//! `refs/heads/harness/<runId>`, beside `<top>/src/`, the head tree. The flow:
//!
//! 1. [`Git::check`]: git must run (on macOS without the Xcode command line tools `/usr/bin/git` is a stub that exits non-zero), and
//!    the destination must be free, both before any byte is downloaded.
//! 2. The zip comes from the pull flow ([`crate::pull`]): the newest `zip` artifact, else the newest `partial` one, with ranged GETs,
//!    resume and the SHA-256 check. It stays in the download directory.
//! 3. [`clone_zip`] reads only `repo.bundle` out of the zip, into a private staging directory next to the destination
//!    (`.ham-clone-<runId>-<nonce>`), never extracting `src/`. `git init`, `git bundle verify`, `git fetch` of
//!    `refs/heads/harness/<runId>` and a checkout of that branch build the repository in the staging directory: no remote is ever
//!    created, so there is nothing to undo, and the reflog never names the staging path.
//! 4. The proof: every file of the zip's `src/` is hashed as a git blob (SHA-1 over `blob <len>\0` and the bytes, streamed from the
//!    zip) and compared, by path, blob id and mode, with `git ls-tree -r -z HEAD` of the clone; HEAD must be attached to
//!    `harness/<runId>` and `git status` must be clean. Any difference stops the command.
//! 5. Publication is one `rename` of the staging directory onto the destination (which may exist only as an empty directory, other
//!    than the one the caller is in: that is refused with the command to run instead, since the rename would leave the caller's
//!    shell in a deleted directory; an existing empty destination keeps its permission bits, which the clone takes after the move so that
//!    a mode without write permission, 500 or 555, does not stop it). Before the rename succeeds the destination is untouched; after it
//!    the clone is published, and a failure to remove the staging directory, or to set the bits, is a warning in [`Cloned::warnings`],
//!    not an error.
//!
//! Every git command runs with the machine's git configuration shut out: all `GIT_*` variables are removed (except `GIT_EXEC_PATH`),
//! `HOME` and `XDG_CONFIG_HOME` point at an empty directory, the system and global configuration are disabled and the template
//! directory is empty, so a user's hooks, filters, `url.insteadOf`, `core.fsmonitor` or template cannot take part. This is
//! isolation of configuration, not a sandbox: the delivered repository is data the harness built from agents' commits, and `ham clone`
//! runs none of its code.

use crate::api::Api;
use crate::http::ControlPlane;
use crate::pull::{PullError, Pulled, download_with, local_name, select, sha256_file};
use ham_core::command::PullArtifact;
use harness_protocol::api::v1::ArtifactView;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use zip::ZipArchive;

/// What `ham clone` can fail on. The text is for the person at the terminal: what happened and what to do.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error(transparent)]
    Pull(#[from] PullError),
    /// The run has neither a `zip` nor a `partial` artifact.
    #[error("{0}")]
    NoDelivery(String),
    /// git cannot run.
    #[error("{0}")]
    NoGit(String),
    /// The run id is not one `ham runs` prints.
    #[error("{0}")]
    RunId(String),
    /// The destination is a file, a symbolic link or a directory that is not empty.
    #[error("{0}")]
    Dest(String),
    /// The zip is unreadable or does not hold exactly one `repo.bundle`.
    #[error("{0}")]
    Zip(String),
    /// The bundle does not verify, does not unpack, or lacks the run's branch.
    #[error("{0}")]
    Bundle(String),
    /// Another git command failed.
    #[error("{0}")]
    Git(String),
    /// The repository's head tree is not the zip's `src/`.
    #[error("{0}")]
    TreeMismatch(String),
    /// The checkout is not clean.
    #[error("{0}")]
    Dirty(String),
    #[error("io: {0}")]
    Io(String),
}

fn io(path: &Path, e: &std::io::Error) -> CloneError {
    CloneError::Io(format!("{}: {e}", path.display()))
}

/// The git program and how it is run.
#[derive(Clone, Debug)]
pub struct Git {
    program: OsString,
}

impl Git {
    /// The `git` on `PATH`.
    pub fn system() -> Self {
        Self::with_program("git")
    }

    /// Another program (tests: one that does not exist, or one that always fails).
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// The program this runs (`ham push` runs the push itself, with the user's own configuration).
    pub fn program(&self) -> &std::ffi::OsStr {
        &self.program
    }

    /// `git --version`: the version line, or [`CloneError::NoGit`] in plain words.
    pub fn check(&self) -> Result<String, CloneError> {
        let mut c = Command::new(&self.program);
        c.arg("--version").stdin(Stdio::null());
        clear_git_env(&mut c);
        let why = match c.output() {
            Ok(o) if o.status.success() => {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if v.starts_with("git version") {
                    return Ok(v);
                }
                format!("it printed {v:?}")
            }
            Ok(o) => format!(
                "it exited with {}{}",
                o.status
                    .code()
                    .map_or_else(|| "a signal".to_string(), |c| c.to_string()),
                first_line(&o.stderr).map_or(String::new(), |l| format!(": {l}"))
            ),
            Err(e) => e.to_string(),
        };
        Err(CloneError::NoGit(format!(
            "git is not available ({}: {why}), and ham clone needs it to build the repository. On macOS, install the Xcode command line tools with xcode-select --install; on Linux, install the git package with your package manager. Nothing was cloned.",
            self.program.to_string_lossy()
        )))
    }
}

fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
}

/// Removes every `GIT_*` variable but `GIT_EXEC_PATH` (a relocated git needs it) from the child's environment.
fn clear_git_env(c: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if name.starts_with("GIT_") && name != "GIT_EXEC_PATH" {
            c.env_remove(&k);
        }
    }
}

/// A finished git command: its output, or the first line of its stderr.
struct Out {
    stdout: Vec<u8>,
}

/// The staging directory: `<parent>/.ham-clone-<runId>-<nonce>` with a private mode, holding the bundle, an empty home and
/// template directory for git, and the work tree under construction.
struct Stage {
    root: PathBuf,
    home: PathBuf,
    template: PathBuf,
    bundle: PathBuf,
    work: PathBuf,
    /// The directory is removed when the stage is dropped (an error, a panic) unless [`Stage::discard`] already did.
    armed: bool,
}

impl Stage {
    fn create(parent: &Path, run_id: &str) -> Result<Self, CloneError> {
        std::fs::create_dir_all(parent).map_err(|e| {
            CloneError::Dest(format!(
                "cannot create {}: {e}. Name a directory you can write to.",
                parent.display()
            ))
        })?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root = parent.join(format!(
            ".ham-clone-{run_id}-{}-{nanos}",
            std::process::id()
        ));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .map_err(|e| {
                    CloneError::Dest(format!(
                        "cannot create a staging directory in {}: {e}. Name a directory you can write to.",
                        parent.display()
                    ))
                })?;
        }
        // From here on `Drop` removes the directory, so a failure to make the rest of it leaves nothing behind.
        let s = Self {
            home: root.join("home"),
            template: root.join("template"),
            bundle: root.join("repo.bundle"),
            work: root.join("work"),
            root,
            armed: true,
        };
        for d in [&s.home, &s.template] {
            std::fs::create_dir(d).map_err(|e| io(d, &e))?;
        }
        Ok(s)
    }

    /// Removes the staging directory now; the message when that fails.
    fn discard(&mut self) -> Option<String> {
        self.armed = false;
        std::fs::remove_dir_all(&self.root).err().map(|e| {
            format!(
                "could not remove the staging directory {}: {e}",
                self.root.display()
            )
        })
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl Git {
    /// One git command in `cwd` with the configuration shut out; `what` names it in an error.
    fn run(&self, stage: &Stage, cwd: &Path, args: &[OsString], what: &str) -> Result<Out, String> {
        let mut c = Command::new(&self.program);
        clear_git_env(&mut c);
        c.args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.logAllRefUpdates=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("HOME", &stage.home)
        .env("XDG_CONFIG_HOME", stage.home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
        let o = c
            .output()
            .map_err(|e| format!("{what}: {}: {e}", self.program.to_string_lossy()))?;
        if o.status.success() {
            Ok(Out { stdout: o.stdout })
        } else {
            Err(format!(
                "{what}: {}",
                first_line(&o.stderr)
                    .or_else(|| first_line(&o.stdout))
                    .unwrap_or_else(|| format!("exit {}", o.status.code().unwrap_or(-1)))
            ))
        }
    }
}

/// What the zip says about one path of `src/`, or what a checkout holds at it: the git mode and the blob id of the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub mode: &'static str,
    pub id: String,
}

/// What `git ls-tree` says about one path.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    id: String,
}

/// The git blob id of `content`: SHA-1 over `blob <len>\0` and the bytes.
pub fn blob_id(content: &[u8]) -> String {
    // nosemgrep: insecure-hashes -- git names a file by SHA-1 over the blob header and its bytes, ham clone computes that id to compare the files of the zip with git ls-tree of the clone, and the zip itself is checked with SHA-256, so this SHA-1 authenticates nothing
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", content.len()).as_bytes());
    h.update(content);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The git mode of a zip entry's Unix mode: `120000` for a symbolic link, `100755` for a regular file with any execute bit,
/// else `100644`; an entry with no Unix mode is a regular file. A directory, device, fifo or socket is not a source file.
pub fn git_mode(unix_mode: Option<u32>) -> Result<&'static str, String> {
    let m = unix_mode.unwrap_or(0o100_644);
    match m & 0o170_000 {
        0o120_000 => Ok("120000"),
        0 | 0o100_000 => Ok(if m & 0o111 == 0 { "100644" } else { "100755" }),
        other => Err(format!("file type {other:o} is not a source file")),
    }
}

/// `<repo>` of a zip's top directory `<repo>-<runId>-<sha7>`; the run id when that is not a plain, single path component
/// (letters, digits, `.`, `_` and `-`, not starting with `.` or `-`).
pub fn default_dir_name(top: &str, run_id: &str) -> String {
    let safe = |r: &str| {
        !r.is_empty()
            && r.len() <= 100
            && !r.starts_with(['.', '-'])
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    top.find(&format!("-{run_id}-"))
        .map(|i| &top[..i])
        .filter(|r| safe(r))
        .map_or_else(|| run_id.to_string(), str::to_string)
}

/// A path for a message: text that is valid UTF-8 as it is, and each byte that is not as `\xNN`. A name that git keeps in other bytes (a
/// Latin-1 name from a Linux file system) never turns into a replacement character, which would show two different names alike.
fn shown_name(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out
}

/// `git status --porcelain=v1 -z --untracked-files=all` must be empty.
pub fn check_clean(status: &[u8]) -> Result<(), CloneError> {
    let paths: Vec<String> = status
        .split(|b| *b == 0)
        .filter(|r| r.len() > 3)
        .map(|r| shown_name(&r[3..]))
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let shown = paths.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
    let more = paths.len().saturating_sub(8);
    Err(CloneError::Dirty(format!(
        "git reports the new checkout as not clean ({shown}{}). The usual cause is a case-insensitive file system (two paths that differ only by case) or a path it cannot hold. Nothing was published; the zip is kept, and unzip reads it as it is.",
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )))
}

/// The request of [`clone_zip`].
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub run_id: &'a str,
    /// The downloaded, verified zip.
    pub zip: &'a Path,
    /// Where to clone; relative to `cwd`. None: `<cwd>/<repo name>`.
    pub dest: Option<&'a Path>,
    pub cwd: &'a Path,
    /// The zip is a `partial` artifact (a run that did not complete).
    pub partial: bool,
}

/// A published clone.
#[derive(Clone, Debug, PartialEq)]
pub struct Cloned {
    pub dir: PathBuf,
    pub branch: String,
    pub head: String,
    pub commits: usize,
    /// Files of the head tree (symbolic links included).
    pub files: usize,
    /// The zip's top directory.
    pub top: String,
    pub partial: bool,
    pub zip: PathBuf,
    pub zip_sha256: String,
    /// Things that went wrong after the clone was published.
    pub warnings: Vec<String>,
}

fn open_zip(path: &Path) -> Result<ZipArchive<BufReader<File>>, CloneError> {
    let f = File::open(path).map_err(|e| io(path, &e))?;
    ZipArchive::new(BufReader::new(f))
        .map_err(|e| CloneError::Zip(format!("{} is not a readable zip: {e}", path.display())))
}

/// The index of the zip's one `<top>/repo.bundle`, and `<top>`.
fn find_bundle(
    zip: &mut ZipArchive<BufReader<File>>,
    path: &Path,
) -> Result<(usize, String), CloneError> {
    let mut found: Vec<(usize, String)> = Vec::new();
    for i in 0..zip.len() {
        let f = zip
            .by_index_raw(i)
            .map_err(|e| CloneError::Zip(format!("{}: {e}", path.display())))?;
        let name = f.name_raw();
        let Some(slash) = name.iter().position(|b| *b == b'/') else {
            continue;
        };
        if &name[slash + 1..] == b"repo.bundle" && !f.is_dir() {
            // The top directory's bytes are matched again below as a prefix, so a name that is not UTF-8 cannot stand for itself.
            let top = std::str::from_utf8(&name[..slash]).map_err(|_| {
                CloneError::Zip(format!(
                    "{} names its top directory in bytes that are not UTF-8, so it is not a delivery ham clone can read. Unzip it to read the code: unzip {}",
                    path.display(),
                    path.display()
                ))
            })?;
            found.push((i, top.to_string()));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(CloneError::Zip(format!(
            "{} holds no repo.bundle, so it cannot be cloned (a delivery from before the bundle was added). Unzip it to read the code: unzip {}",
            path.display(),
            path.display()
        ))),
        _ => Err(CloneError::Zip(format!(
            "{} holds more than one repo.bundle ({}), so it is not a delivery ham clone can read. Unzip it to read the code: unzip {}",
            path.display(),
            found
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            path.display()
        ))),
    }
}

/// Copies the bundle entry to `to`, checking its length.
fn extract_bundle(
    zip: &mut ZipArchive<BufReader<File>>,
    index: usize,
    to: &Path,
) -> Result<(), CloneError> {
    let mut entry = zip
        .by_index(index)
        .map_err(|e| CloneError::Zip(format!("repo.bundle: {e}")))?;
    let size = entry.size();
    let mut out = File::create(to).map_err(|e| io(to, &e))?;
    copy_declared(&mut entry, size, &mut out)
}

/// Copies `src` to `out` and checks that it held exactly the `size` bytes its zip header declares.
///
/// zip 9 already fails a read past the declared size (`File is larger than its declared uncompressed size`). The `take` stays as a
/// second bound that does not depend on the zip crate: at most one byte past `size` is read, so an entry that inflates without end
/// costs `size + 1` bytes of disk, not its inflated length, and `n > size` refuses it with its own message.
fn copy_declared<R: Read, W: Write>(src: &mut R, size: u64, out: &mut W) -> Result<(), CloneError> {
    let n = std::io::copy(&mut src.take(size.saturating_add(1)), out)
        .map_err(|e| CloneError::Zip(format!("repo.bundle: {e}")))?;
    if n > size {
        return Err(CloneError::Zip(format!(
            "repo.bundle holds more than the {size} bytes the zip declares"
        )));
    }
    if n != size {
        return Err(CloneError::Zip(format!(
            "repo.bundle is {n} bytes, the zip says {size}"
        )));
    }
    Ok(())
}

/// The git blob id of the `size` bytes `r` yields, streamed through `buf`; an error when it yields more or fewer.
fn stream_blob_id<R: Read>(r: &mut R, size: u64, buf: &mut [u8]) -> Result<String, String> {
    // nosemgrep: insecure-hashes -- git names a file by SHA-1 over the blob header and its bytes, ham clone computes that id to compare the files of the zip with git ls-tree of the clone, and the zip itself is checked with SHA-256, so this SHA-1 authenticates nothing
    let mut h = Sha1::new();
    h.update(format!("blob {size}\0").as_bytes());
    let mut read = 0u64;
    loop {
        let n = r.read(buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n as u64;
        h.update(&buf[..n]);
    }
    if read != size {
        return Err(format!("{read} bytes, expected {size}"));
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Path (raw bytes) to blob of every file under `<top>/src/`, streamed from the zip.
fn src_tree(
    zip: &mut ZipArchive<BufReader<File>>,
    top: &str,
) -> Result<BTreeMap<Vec<u8>, Blob>, CloneError> {
    let prefix = format!("{top}/src/").into_bytes();
    let mut tree = BTreeMap::new();
    let mut buf = vec![0u8; 64 * 1024];
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| CloneError::Zip(format!("entry {i}: {e}")))?;
        let name = f.name_raw().to_vec();
        let Some(rel) = name.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        if rel.is_empty() || f.is_dir() || rel.ends_with(b"/") {
            continue;
        }
        let shown = shown_name(rel);
        let mode = git_mode(f.unix_mode())
            .map_err(|e| CloneError::TreeMismatch(format!("src/{shown}: {e}")))?;
        let size = f.size();
        let id = stream_blob_id(&mut f, size, &mut buf)
            .map_err(|e| CloneError::Zip(format!("src/{shown}: {e}")))?;
        // The zip crate keeps one entry per name (a later entry of the same name replaces the earlier one), so a duplicate cannot
        // reach here; an entry it hid shows as a file of the repository that the zip lacks.
        tree.insert(rel.to_vec(), Blob { mode, id });
    }
    Ok(tree)
}

/// `git ls-tree -r -z --full-tree HEAD` by path (raw bytes); gitlinks (`160000`, which `git archive` writes as empty directories)
/// are left out.
fn parse_ls_tree(out: &[u8]) -> Result<BTreeMap<Vec<u8>, Entry>, CloneError> {
    let mut tree = BTreeMap::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let bad = || {
            CloneError::Git(format!(
                "ls-tree: unexpected record {:?}",
                String::from_utf8_lossy(rec)
            ))
        };
        let tab = rec.iter().position(|b| *b == b'\t').ok_or_else(bad)?;
        let meta = String::from_utf8_lossy(&rec[..tab]).into_owned();
        let mut parts = meta.split(' ');
        let (Some(mode), Some(_kind), Some(id), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        if mode == "160000" {
            continue;
        }
        if id.len() != 40 {
            return Err(CloneError::Git(format!(
                "the repository uses {}-digit object ids; ham clone reads SHA-1 repositories only",
                id.len()
            )));
        }
        tree.insert(
            rec[tab + 1..].to_vec(),
            Entry {
                mode: mode.to_string(),
                id: id.to_string(),
            },
        );
    }
    Ok(tree)
}

/// The differences between the zip's `src/` and the repository's head tree, as lines (at most 8 and a count of the rest).
fn differences(want: &BTreeMap<Vec<u8>, Blob>, have: &BTreeMap<Vec<u8>, Entry>) -> Vec<String> {
    let name = shown_name;
    let mut diffs = Vec::new();
    for (path, w) in want {
        match have.get(path) {
            None => diffs.push(format!("only in the zip: {}", name(path))),
            Some(h) if h.mode != w.mode => diffs.push(format!(
                "{}: mode {} in the zip, {} in the repository",
                name(path),
                w.mode,
                h.mode
            )),
            Some(h) if h.id != w.id => diffs.push(format!(
                "{}: content differs (blob {} in the zip, {} in the repository)",
                name(path),
                &w.id[..12],
                &h.id[..12]
            )),
            Some(_) => {}
        }
    }
    for path in have.keys().filter(|p| !want.contains_key(*p)) {
        diffs.push(format!("only in the repository: {}", name(path)));
    }
    diffs
}

/// The paths, from either tree, whose bytes are not valid UTF-8, shown with their bytes escaped (sorted, each once). A zip entry name is
/// text, so such a name cannot be the same path on both sides.
fn non_utf8_names(want: &BTreeMap<Vec<u8>, Blob>, have: &BTreeMap<Vec<u8>, Entry>) -> Vec<String> {
    let mut names: Vec<String> = want
        .keys()
        .chain(have.keys())
        .filter(|p| std::str::from_utf8(p).is_err())
        .map(|p| shown_name(p))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The refusal for a head tree that is not the zip's `src/`. `non_utf8` are the paths of either tree whose bytes are not UTF-8: when there
/// are some they are the cause that is named, and the hint about an earlier release of the packager, which is about links and conversion
/// rules, is left out.
fn tree_mismatch(diffs: &[String], non_utf8: &[String]) -> CloneError {
    let shown = diffs.iter().take(8).cloned().collect::<Vec<_>>().join("; ");
    let more = diffs.len().saturating_sub(8);
    let cause = match non_utf8.first() {
        None => "A zip made by an earlier release can differ this way when the repository has a symbolic link or a .gitattributes conversion rule (the packager now keeps src/ verbatim); unzip reads it as it is.".to_string(),
        Some(first) => format!(
            "The trees hold {} file name{} that {} not valid UTF-8, for example {first}. A zip entry name is text, so src/ cannot carry such a name as git has it, and the two trees cannot be proven equal; unzip reads the zip as it is.",
            non_utf8.len(),
            if non_utf8.len() == 1 { "" } else { "s" },
            if non_utf8.len() == 1 { "is" } else { "are" },
        ),
    };
    CloneError::TreeMismatch(format!(
        "the repository in repo.bundle is not the tree in the zip's src/ ({} difference{}): {shown}{}. Nothing was published. {cause}",
        diffs.len(),
        if diffs.len() == 1 { "" } else { "s" },
        if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        }
    ))
}

/// Every file and symbolic link below `root` (the top-level `.git` left out) by path (raw bytes, `/`-separated): the git mode its
/// type and execute bit give, and the blob id of the bytes on disk (a link's target for a link). Empty directories, which only a
/// gitlink leaves, are not entries.
pub fn worktree(root: &Path) -> Result<BTreeMap<Vec<u8>, Blob>, CloneError> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut stack = vec![(root.to_path_buf(), Vec::<u8>::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, &e))? {
            let entry = entry.map_err(|e| io(&dir, &e))?;
            let name = entry.file_name();
            if rel.is_empty() && name.as_bytes() == b".git" {
                continue;
            }
            let mut path = rel.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name.as_bytes());
            let full = entry.path();
            let meta = std::fs::symlink_metadata(&full).map_err(|e| io(&full, &e))?;
            let kind = meta.file_type();
            if kind.is_dir() {
                stack.push((full, path));
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&full).map_err(|e| io(&full, &e))?;
                out.insert(
                    path,
                    Blob {
                        mode: "120000",
                        id: blob_id(target.as_os_str().as_bytes()),
                    },
                );
            } else if kind.is_file() {
                let mut f = File::open(&full).map_err(|e| io(&full, &e))?;
                let id = stream_blob_id(&mut f, meta.len(), &mut buf)
                    .map_err(|e| CloneError::Io(format!("{}: {e}", full.display())))?;
                let mode = if meta.permissions().mode() & 0o111 == 0 {
                    "100644"
                } else {
                    "100755"
                };
                out.insert(path, Blob { mode, id });
            } else {
                return Err(CloneError::TreeMismatch(format!(
                    "{} is not a file, a directory or a symbolic link",
                    full.display()
                )));
            }
        }
    }
    Ok(out)
}

/// The checkout on disk against the zip's `src/`: the same paths, kinds and modes, as git itself cannot vouch for a file system that
/// folds two paths into one (a case-insensitive one gives `A` and `a` one file and a clean status). Paths whose bytes differ from the
/// zip's, which a `.gitattributes` rule can cause on any system (line endings, `ident`, an encoding), are returned, not refused: their
/// committed blobs are already proven equal.
pub fn check_worktree(
    root: &Path,
    want: &BTreeMap<Vec<u8>, Blob>,
) -> Result<Vec<String>, CloneError> {
    let disk = worktree(root)?;
    let name = shown_name;
    let mut diffs = Vec::new();
    let mut converted = Vec::new();
    for (path, w) in want {
        match disk.get(path) {
            None => diffs.push(format!("only in the zip, not on disk: {}", name(path))),
            Some(d) if d.mode != w.mode => diffs.push(format!(
                "{}: mode {} in the zip, {} on disk",
                name(path),
                w.mode,
                d.mode
            )),
            Some(d) if d.id != w.id => converted.push(name(path)),
            Some(_) => {}
        }
    }
    for path in disk.keys().filter(|p| !want.contains_key(*p)) {
        diffs.push(format!("only on disk, not in the zip: {}", name(path)));
    }
    if diffs.is_empty() {
        return Ok(converted);
    }
    let shown = diffs.iter().take(8).cloned().collect::<Vec<_>>().join("; ");
    let more = diffs.len().saturating_sub(8);
    Err(CloneError::TreeMismatch(format!(
        "the checkout on disk is not the zip's src/ ({} difference{}): {shown}{}. A file system that treats two paths as one (not case-sensitive, or folding Unicode forms) is the usual cause. Nothing was published; the zip is kept, and unzip reads it as it is.",
        diffs.len(),
        if diffs.len() == 1 { "" } else { "s" },
        if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        }
    )))
}

/// A run id is the ULID `ham runs` prints; it names a branch and a staging directory, so it must be one plain path component.
fn check_run_id(run_id: &str) -> Result<(), CloneError> {
    if (1..=64).contains(&run_id.len())
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(CloneError::RunId(format!(
            "{run_id:?} is not a run id; use the id `ham runs` prints"
        )))
    }
}

/// The destination must not exist, or be an empty directory other than the one the caller is in: the work tree is renamed onto the
/// destination, which would leave a shell in the caller's directory in a deleted directory.
fn check_dest(dest: &Path, run_id: &str, cwd: &Path) -> Result<(), CloneError> {
    let again =
        format!("Give a directory that does not exist or is empty: ham clone {run_id} <directory>");
    match std::fs::symlink_metadata(dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CloneError::Dest(format!("{}: {e}", dest.display()))),
        Ok(m) if m.file_type().is_symlink() => Err(CloneError::Dest(format!(
            "{} is a symbolic link, and ham clone does not follow one. {again}",
            dest.display()
        ))),
        Ok(m) if m.is_dir() => {
            let mut entries = std::fs::read_dir(dest).map_err(|e| io(dest, &e))?;
            if entries.next().is_some() {
                Err(CloneError::Dest(format!(
                    "{} already exists and is not empty. {again}",
                    dest.display()
                )))
            } else if is_same_dir(&m, cwd) {
                Err(CloneError::Dest(format!(
                    "{} is the directory you are in. ham clone builds the repository beside its destination and moves it into place, which would leave your shell in a deleted directory. {again}",
                    dest.display()
                )))
            } else {
                Ok(())
            }
        }
        Ok(_) => Err(CloneError::Dest(format!(
            "{} exists and is not a directory. {again}",
            dest.display()
        ))),
    }
}

/// Whether `dest` (the metadata of a directory) is the directory `cwd` names, by device and inode, so `.`, `../here` and an absolute
/// path to it are all the same directory. A `cwd` that cannot be looked at is not the same.
fn is_same_dir(dest: &std::fs::Metadata, cwd: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(cwd).is_ok_and(|c| c.dev() == dest.dev() && c.ino() == dest.ino())
}

/// The destination for `req`: the given one under `cwd`, else `<cwd>/<name>`, absolute and without `.` components (`dir/.` is `dir`).
fn destination(req: &Request<'_>, name: &str) -> PathBuf {
    let joined = match req.dest {
        Some(d) => req.cwd.join(d),
        None => req.cwd.join(name),
    };
    let absolute = std::path::absolute(&joined).unwrap_or(joined);
    absolute.components().collect()
}

/// Builds the repository of the downloaded `req.zip` and publishes it (see the module doc).
pub fn clone_zip(git: &Git, req: &Request<'_>) -> Result<Cloned, CloneError> {
    check_run_id(req.run_id)?;
    git.check()?;
    let mut zip = open_zip(req.zip)?;
    let (index, top) = find_bundle(&mut zip, req.zip)?;
    let dest = destination(req, &default_dir_name(&top, req.run_id));
    check_dest(&dest, req.run_id, req.cwd)?;
    let parent = dest
        .parent()
        .map_or_else(|| std::path::PathBuf::from("/"), Path::to_path_buf);
    // Dropping the stage (an error below, a panic) removes the staging directory.
    let mut stage = Stage::create(&parent, req.run_id)?;
    let mut cloned = build(git, req, &mut zip, index, &top, &stage)?;
    // Publication: before this rename succeeds the destination is untouched.
    check_dest(&dest, req.run_id, req.cwd)?;
    // An existing empty destination is a directory the person made: the clone keeps its permission bits (`chmod 700` stays 700). The bits
    // are applied after the move: a directory cannot be moved out of the staging directory without write permission, so a
    // staged directory that took mode 500 or 555 first could not be renamed. Before the move it gains the owner's write bit and nothing
    // else, which is never wider than what the person chose for anyone but the owner.
    let kept = match std::fs::symlink_metadata(&dest) {
        Ok(m) if m.is_dir() => Some(m.permissions()),
        _ => None,
    };
    if let Some(kept) = &kept {
        use std::os::unix::fs::PermissionsExt;
        let writable = std::fs::Permissions::from_mode((kept.mode() & 0o7777) | 0o200);
        std::fs::set_permissions(&stage.work, writable).map_err(|e| io(&stage.work, &e))?;
    }
    std::fs::rename(&stage.work, &dest).map_err(|e| {
        // The staging directory must stay removable whatever mode the destination had.
        let _ = std::fs::set_permissions(
            &stage.work,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        );
        CloneError::Dest(format!(
            "could not move the clone to {}: {e}. Nothing was published.",
            dest.display()
        ))
    })?;
    cloned.dir = dest;
    if let Some(kept) = kept
        && let Err(e) = std::fs::set_permissions(&cloned.dir, kept)
    {
        cloned.warnings.push(format!(
            "could not give {} the permission bits of the directory it replaced: {e}",
            cloned.dir.display()
        ));
    }
    if let Some(w) = stage.discard() {
        cloned.warnings.push(w);
    }
    Ok(cloned)
}

/// The staging work: bundle out of the zip, repository, proof. Returns the clone as it will be published.
fn build(
    git: &Git,
    req: &Request<'_>,
    zip: &mut ZipArchive<BufReader<File>>,
    index: usize,
    top: &str,
    stage: &Stage,
) -> Result<Cloned, CloneError> {
    let branch = format!("harness/{}", req.run_id);
    let refname = format!("refs/heads/{branch}");
    extract_bundle(zip, index, &stage.bundle)?;
    let want = src_tree(zip, top)?;
    let in_bundle = |e: String| {
        CloneError::Bundle(format!(
            "the delivered repo.bundle does not verify or unpack ({e}). The zip itself was downloaded and checked; unzip it to read the code as it is."
        ))
    };
    macro_rules! os {
        ($($a:expr),* $(,)?) => { [$(std::ffi::OsStr::new($a).to_os_string()),*] };
    }
    let mut template = OsString::from("--template=");
    template.push(&stage.template);

    git.run(
        stage,
        &stage.root,
        // `-c init.defaultBranch` rather than `--initial-branch`: it reaches git before 2.28 too, where it is ignored.
        &[
            OsString::from("-c"),
            OsString::from("init.defaultBranch=ham-stage"),
            OsString::from("init"),
            OsString::from("-q"),
            template,
            stage.work.clone().into_os_string(),
        ],
        "git init",
    )
    .map_err(CloneError::Git)?;
    git.run(
        stage,
        &stage.work,
        &os!["bundle", "verify", &stage.bundle],
        "git bundle verify",
    )
    .map_err(in_bundle)?;
    let heads = git
        .run(
            stage,
            &stage.work,
            &os!["bundle", "list-heads", &stage.bundle, &refname],
            "git bundle list-heads",
        )
        .map_err(in_bundle)?;
    let advertised = String::from_utf8_lossy(&heads.stdout)
        .lines()
        .find_map(|l| {
            let (sha, name) = l.split_once(' ')?;
            (name.trim() == refname).then(|| sha.to_string())
        })
        .ok_or_else(|| {
            CloneError::Bundle(format!(
                "the delivered repo.bundle holds no {branch} (the branch of run {}), so it is not this run's repository. Unzip the zip to read the code as it is.",
                req.run_id
            ))
        })?;
    git.run(
        stage,
        &stage.work,
        &os![
            "fetch",
            "-q",
            "--no-tags",
            "--no-recurse-submodules",
            &stage.bundle,
            &format!("{refname}:{refname}"),
        ],
        "git fetch",
    )
    .map_err(in_bundle)?;
    // The fetch leaves a note of where it fetched from, which names the staging directory.
    let _ = std::fs::remove_file(stage.work.join(".git/FETCH_HEAD"));
    git.run(
        stage,
        &stage.work,
        &os!["symbolic-ref", "HEAD", &refname],
        "git symbolic-ref",
    )
    .map_err(CloneError::Git)?;
    git.run(
        stage,
        &stage.work,
        &os!["reset", "-q", "--hard", "HEAD"],
        "git checkout",
    )
    .map_err(CloneError::Git)?;

    let text = |args: &[OsString], what: &str| -> Result<String, CloneError> {
        git.run(stage, &stage.work, args, what)
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .map_err(CloneError::Git)
    };
    let attached = text(&os!["symbolic-ref", "-q", "HEAD"], "git symbolic-ref")?;
    if attached != refname {
        return Err(CloneError::Git(format!(
            "the checkout is on {attached}, not {refname}"
        )));
    }
    let head = text(
        &os!["rev-parse", "--verify", "HEAD^{commit}"],
        "git rev-parse",
    )?;
    if head != advertised {
        return Err(CloneError::Git(format!(
            "the checkout is at {head}, but the bundle advertises {advertised} for {branch}"
        )));
    }
    let commits = text(&os!["rev-list", "--count", "HEAD"], "git rev-list")?
        .parse::<usize>()
        .map_err(|e| CloneError::Git(format!("git rev-list --count: {e}")))?;
    let tree = git
        .run(
            stage,
            &stage.work,
            &os!["ls-tree", "-r", "-z", "--full-tree", "HEAD"],
            "git ls-tree",
        )
        .map_err(CloneError::Git)?;
    let have = parse_ls_tree(&tree.stdout)?;
    let diffs = differences(&want, &have);
    if !diffs.is_empty() {
        return Err(tree_mismatch(&diffs, &non_utf8_names(&want, &have)));
    }
    let status = git
        .run(
            stage,
            &stage.work,
            &os!["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            "git status",
        )
        .map_err(CloneError::Git)?;
    check_clean(&status.stdout)?;
    // Git's own view is clean; now the disk: every path, kind and mode of the zip is there once, and nothing else is.
    let converted = check_worktree(&stage.work, &want)?;
    let mut warnings = Vec::new();
    if !converted.is_empty() {
        warnings.push(format!(
            "{} file{} differ from the zip's bytes because a .gitattributes rule converts them on checkout (line endings, ident or an encoding), for example {}; their committed blobs equal the zip's",
            converted.len(),
            if converted.len() == 1 { "" } else { "s" },
            converted.first().map_or("", String::as_str)
        ));
    }

    Ok(Cloned {
        dir: stage.work.clone(),
        branch,
        head,
        commits,
        files: have.len(),
        top: top.to_string(),
        partial: req.partial,
        zip: req.zip.to_path_buf(),
        zip_sha256: sha256_file(req.zip).map_err(CloneError::Pull)?,
        warnings,
    })
}

/// The request of [`clone_run`].
#[derive(Clone, Copy, Debug)]
pub struct RunRequest<'a> {
    pub run_id: &'a str,
    /// Where to clone; relative to `cwd`. None: `<cwd>/<repo name>`.
    pub dest: Option<&'a Path>,
    /// Where the zip is downloaded (the pull rules).
    pub downloads: &'a Path,
    pub cwd: &'a Path,
    /// The range size of the download.
    pub chunk: u64,
}

/// The artifact to clone: the newest `zip`, else the newest `partial`. A `zip` that exists is never replaced by a `partial` because
/// its download failed; that is an error.
pub fn choose(
    artifacts: &[ArtifactView],
    run_id: &str,
) -> Result<(ArtifactView, bool), CloneError> {
    if let Ok(a) = select(artifacts, &PullArtifact::Zip) {
        return Ok((a, false));
    }
    select(artifacts, &PullArtifact::Partial)
        .map(|a| (a, true))
        .map_err(|_| {
            CloneError::NoDelivery(format!(
                "run {run_id} has no zip or partial zip yet; ham clone needs a delivery. Follow the run with ham watch {run_id}."
            ))
        })
}

/// Downloads the run's zip (or partial zip) like `ham pull` and clones it. `notice` receives what the person should see before the
/// clone starts (a partial delivery). Git and the destination are checked before the download.
pub async fn clone_run<P: ControlPlane>(
    api: &Api<P>,
    git: &Git,
    req: &RunRequest<'_>,
    notice: &mut (dyn FnMut(&str) + Send),
) -> Result<(Cloned, Pulled), CloneError> {
    check_run_id(req.run_id)?;
    git.check()?;
    let list = api
        .list_artifacts(req.run_id)
        .await
        .map_err(PullError::from)?;
    let (artifact, partial) = choose(&list, req.run_id)?;
    if partial {
        notice(&format!(
            "run {} did not complete: cloning its partial delivery, the work so far and not a finished result",
            req.run_id
        ));
    }
    // The zip is named `<top>.zip`, so the default destination is known before the download.
    let name = local_name(req.run_id, &artifact);
    let guess = Request {
        run_id: req.run_id,
        zip: Path::new(&name),
        dest: req.dest,
        cwd: req.cwd,
        partial,
    };
    check_dest(
        &destination(
            &guess,
            &default_dir_name(name.strip_suffix(".zip").unwrap_or(&name), req.run_id),
        ),
        req.run_id,
        req.cwd,
    )?;
    let pulled = download_with(api, req.run_id, &artifact, req.downloads, req.chunk).await?;
    let (git, zip, run_id, dest, cwd) = (
        git.clone(),
        pulled.path.clone(),
        req.run_id.to_string(),
        req.dest.map(Path::to_path_buf),
        req.cwd.to_path_buf(),
    );
    let cloned = tokio::task::spawn_blocking(move || {
        clone_zip(
            &git,
            &Request {
                run_id: &run_id,
                zip: &zip,
                dest: dest.as_deref(),
                cwd: &cwd,
                partial,
            },
        )
    })
    .await
    .map_err(|e| CloneError::Io(format!("the clone task failed: {e}")))??;
    Ok((cloned, pulled))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rec(mode: &str, id: &str, path: &str) -> Vec<u8> {
        format!("{mode} blob {id}\t{path}").into_bytes()
    }

    #[test]
    fn ls_tree_records_are_read_by_raw_path_and_gitlinks_are_left_out() {
        let sha = "a".repeat(40);
        let mut out = rec("100644", &sha, "calc.py");
        out.push(0);
        out.extend(rec("160000", &sha, "vendor/sub"));
        out.push(0);
        out.extend(rec("100755", &sha, "dir/with space/run.sh"));
        out.push(0);
        let tree = parse_ls_tree(&out).unwrap();
        assert_eq!(tree.len(), 2, "{tree:?}");
        assert_eq!(tree[b"calc.py".as_slice()].mode, "100644");
        assert_eq!(tree[b"dir/with space/run.sh".as_slice()].mode, "100755");
    }

    #[test]
    fn a_malformed_ls_tree_record_and_a_sha256_repository_are_errors() {
        for bad in [
            b"no tab here".to_vec(),
            b"100644 blob\tx".to_vec(),
            b"a b c d\tx".to_vec(),
        ] {
            let err = parse_ls_tree(&bad).unwrap_err();
            assert!(matches!(err, CloneError::Git(_)), "{err:?}");
            assert!(err.to_string().contains("unexpected record"), "{err}");
        }
        let long = rec("100644", &"b".repeat(64), "f");
        let err = parse_ls_tree(&long).unwrap_err();
        assert!(err.to_string().contains("SHA-1 repositories only"), "{err}");
    }

    /// A deflate entry whose header declares 16 bytes but inflates to 1 MiB (with the CRC of the whole inflated data, so the checksum
    /// would pass): the entry is refused and at most one byte past the declared size reaches the disk. zip 9 refuses it first (its reader
    /// fails a read past the declared size); `copy_declared`'s own bound is covered by the `copy_declared_*` cases below, which feed it a
    /// reader that ignores the declared size.
    #[test]
    fn a_bundle_entry_that_inflates_past_its_declared_size_is_cut_off_and_refused() {
        use std::io::{Cursor, Write};
        // nosemgrep: temp-dir -- scratch directory of a unit test, named with the process id and the clock, it holds a generated test bundle only
        let dir = std::env::temp_dir().join(format!(
            "ham-unit-bomb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("t/repo.bundle", opts).unwrap();
        zw.write_all(&vec![0u8; 1 << 20]).unwrap();
        let mut bytes = zw.finish().unwrap().into_inner();
        // Declare 16 bytes in the local header (sizes at +18 and +22) and the central directory (+20 and +24).
        let at = |sig: &[u8; 4]| bytes.windows(4).rposition(|w| w == sig).unwrap();
        let (local, central) = (at(b"PK\x03\x04"), at(b"PK\x01\x02"));
        bytes[local + 22..local + 26].copy_from_slice(&16u32.to_le_bytes());
        bytes[central + 24..central + 28].copy_from_slice(&16u32.to_le_bytes());
        let zip_path = dir.join("bomb.zip");
        std::fs::write(&zip_path, &bytes).unwrap();
        let mut zip = open_zip(&zip_path).unwrap();
        let (index, _) = find_bundle(&mut zip, &zip_path).unwrap();
        assert_eq!(zip.by_index(index).unwrap().size(), 16);
        let to = dir.join("out.bundle");
        let err = extract_bundle(&mut zip, index, &to).unwrap_err();
        assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
        assert!(
            std::fs::metadata(&to).unwrap().len() <= 17,
            "wrote {} bytes for a declared size of 16",
            std::fs::metadata(&to).unwrap().len()
        );
    }

    /// A reader that ignores the declared size and has 1 MiB to give, as a zip reader would without its own size check: the copy reads one
    /// byte past the declared size, stops, and refuses the entry with its own message.
    #[test]
    fn copy_declared_stops_one_byte_past_the_declared_size_and_refuses() {
        let mut src = std::io::Cursor::new(vec![7u8; 1 << 20]);
        let mut out = Vec::new();
        let err = copy_declared(&mut src, 16, &mut out).unwrap_err();
        assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("repo.bundle holds more than the 16 bytes the zip declares"),
            "{err}"
        );
        assert_eq!(
            out.len(),
            17,
            "wrote {} bytes for a declared size of 16",
            out.len()
        );
        assert_eq!(src.position(), 17, "read past the one extra byte");
    }

    #[test]
    fn copy_declared_refuses_an_entry_shorter_than_its_declared_size() {
        let mut src = std::io::Cursor::new(vec![7u8; 10]);
        let mut out = Vec::new();
        let err = copy_declared(&mut src, 16, &mut out).unwrap_err();
        assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("repo.bundle is 10 bytes, the zip says 16"),
            "{err}"
        );
    }

    #[test]
    fn copy_declared_accepts_an_entry_of_exactly_its_declared_size() {
        let data: Vec<u8> = (0u8..16).collect();
        let mut src = std::io::Cursor::new(data.clone());
        let mut out = Vec::new();
        copy_declared(&mut src, 16, &mut out).unwrap();
        assert_eq!(out, data);
        let mut empty = Vec::new();
        copy_declared(&mut std::io::Cursor::new(Vec::new()), 0, &mut empty).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn more_than_eight_differences_are_counted_and_the_rest_summarised() {
        let id = "c".repeat(40);
        let want: BTreeMap<Vec<u8>, Blob> = (0..12)
            .map(|i| {
                (
                    format!("f{i:02}").into_bytes(),
                    Blob {
                        mode: "100644",
                        id: id.clone(),
                    },
                )
            })
            .collect();
        let diffs = differences(&want, &BTreeMap::new());
        assert_eq!(diffs.len(), 12);
        let text = tree_mismatch(&diffs, &[]).to_string();
        assert!(
            text.contains("12 differences") && text.contains("and 4 more"),
            "{text}"
        );
        assert!(
            tree_mismatch(&diffs[..1], &[])
                .to_string()
                .contains("1 difference)"),
            "singular"
        );
    }

    #[test]
    fn a_path_shows_the_bytes_that_are_not_utf_8_as_escapes_and_keeps_the_rest() {
        assert_eq!(shown_name("caf\u{e9}.md".as_bytes()), "caf\u{e9}.md");
        assert_eq!(shown_name(b"caf\xe9.txt"), "caf\\xe9.txt");
        assert_eq!(shown_name(b"a\xff\xfeb"), "a\\xff\\xfeb");
        assert_eq!(shown_name(b""), "");
    }

    #[test]
    fn a_stream_that_is_shorter_or_longer_than_its_declared_size_is_an_error() {
        let mut buf = vec![0u8; 8];
        let mut short: &[u8] = b"abc";
        assert!(
            stream_blob_id(&mut short, 5, &mut buf)
                .unwrap_err()
                .contains("3 bytes, expected 5")
        );
        let mut long: &[u8] = b"abcdef";
        assert!(stream_blob_id(&mut long, 2, &mut buf).is_err());
        let mut exact: &[u8] = b"hello\n";
        assert_eq!(
            stream_blob_id(&mut exact, 6, &mut buf).unwrap(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
