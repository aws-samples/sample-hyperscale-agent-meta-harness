//! `ham update`: the newest version on the main branch of the update source, the changes between, and the update of the backend and
//! then the client through the `setup.sh --update` of the commit ham fetched and checked, after the caller's active runs have ended.
//!
//! The rules that need no process live in `ham_core::update`. This module runs git (isolated from the user's and the system's git
//! configuration), waits for runs (it never cancels one and sends no request that changes one), runs setup with the terminal handed over, and
//! checks both versions afterwards. Its only writes are the clone in the download directory and, when the new client reports another version,
//! the rename of the earlier client that setup kept beside it (`ham.prev`) back into place, and only when that file has the SHA-256 the running
//! ham had before setup started.
//!
//! The clone stays only when the update goes ahead: the newest commit is read in a folder `.ham-update-<process>-<time>` of the
//! download directory, which `--check`, an update that is up to date or refused, and an update the person does not confirm remove again
//! ([`Look`]). An update that goes ahead renames it to `ham-update-<commit>`, the copy setup runs in. A `ham` that is interrupted or
//! killed while it reads the commit, or at the question, cannot remove the folder; the next look removes the folders of looks whose
//! process runs no more ([`sweep_looks`]).
//!
//! An update that deploys nothing on the stage waits for no run. Whether it deploys is setup's decision, so when a run of the caller
//! is active ham asks the setup of the fetched commit for its plan (`--dry-run`: the AWS account is read, nothing is written) and reads the
//! answer with `ham_core::update::plan_deploys_nothing`. Every other answer keeps the wait, and setup decides again when it runs.
//!
//! The look of the terminal app lives here too: [`Git::look`], one listing call of git that asks the update source for the names
//! of its branches and tags, which the rules of `ham_core::update` read for a newer release. Nobody typed a command for it, so it has a
//! command of its own that is closed against the person's settings ([`Git::look_command`]) and a runner of its own with a cap on the output
//! and a bounded wait ([`run_look`]). It writes nothing and makes no call to the stage; `ham update` uses neither.

use crate::api::{Api, ApiError};
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::update::{Change, Plan, Refusal, changes_between, newest_heading, plan};
use harness_protocol::api::v1::RunSummary;
use harness_protocol::goal::GoalStatus;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How often the wait reads the list of runs.
pub const POLL: Duration = Duration::from_secs(30);
/// The limit of the wait when `--wait` is not given, in minutes.
pub const WAIT_MINUTES: u32 = 60;
/// How long ham reads the backend's version after setup ends before it calls a different version a failure: older Lambda versions may
/// still answer for a short while.
pub const BACKEND_RECHECK: Duration = Duration::from_secs(90);
/// The backstop of one git call; the real limit is the stall limit (`http.lowSpeedLimit` for `http.lowSpeedTime`).
pub const GIT_LIMIT: Duration = Duration::from_secs(15 * 60);
/// How long setup may take to print its plan before `ham update` goes on without it and waits for the caller's runs as before. The
/// plan reads the AWS account; about 20 seconds were measured.
pub const PLAN_LIMIT: Duration = Duration::from_secs(180);
/// How long the look of the terminal app waits for git before it ends it. A look that takes this long is a look that failed.
pub const LOOK_LIMIT: Duration = Duration::from_secs(20);
/// The most bytes the look reads of each output of git; more is a look that failed. A listing of the branch and the version tags of
/// a repository with a thousand releases is about 100 KiB.
pub const LOOK_OUTPUT_CAP: usize = 1_048_576;

/// The exit codes of `ham update` that are its own; any other code of setup is passed through. After setup's 3 or 8 ([`SETUP_DONE_CODES`]) the
/// versions are checked first, and a failed check gives `CHECK_FAILED`.
pub mod exit {
    pub const DONE: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const OFF: i32 = 2;
    pub const CHECK_FAILED: i32 = 3;
    pub const CANCELLED: i32 = 4;
    pub const REFUSED: i32 = 7;
}

/// A failure of a step, in words a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError(pub String);

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpdateError {}

fn fail<T>(text: impl Into<String>) -> Result<T, UpdateError> {
    Err(UpdateError(text.into()))
}

/// What one process printed and how it ended.
struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `cmd` with no standard input and both outputs read, killed after `limit`.
fn run_bounded(mut cmd: Command, limit: Duration) -> Result<Ran, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} seconds", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(Ran {
        ok: status.success(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// What the look of the terminal app found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookOutcome {
    /// No look was made: the notice is switched off, `update_source` is off or no source, or `ham update` could not take this config.
    Off,
    /// The version of the newest release the source names.
    Newest(String),
    /// The source answered and names no release: no main branch or no version tag.
    NoRelease,
    /// The source could not be read, did not answer in time, or answered with something that is no listing; or the config could not be
    /// read when the look started, so nothing says whether a look is wanted.
    Failed,
}

/// Reads `pipe` to its end and gives at most `cap` bytes of it. When more arrives it sets `over` and stops reading.
fn read_capped(
    pipe: Option<impl Read>,
    cap: usize,
    over: &std::sync::atomic::AtomicBool,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let Some(mut pipe) = pipe else {
        return bytes;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) if bytes.len() + n > cap => {
                over.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    bytes
}

/// The runner of the look: `cmd` with no standard input, each output read up to `cap` bytes, ended after `limit`. It gives the exit
/// code (`None` for a child ended by a signal) and the standard output; the error output is read, so the child never blocks on it, and
/// dropped.
///
/// It differs from [`run_bounded`] in what a look that runs by itself must not do: keep output without a limit, or wait without a limit for
/// the outputs of a child that has ended. More than `cap` bytes on either output ends the child and is an error. After the child ended by
/// itself, its standard output is waited for only until `limit` has passed since the start, so a helper that keeps the pipe open cannot hold
/// the look; the reader's thread is then left to end when the pipe closes.
#[doc(hidden)]
pub fn run_look(
    mut cmd: Command,
    limit: Duration,
    cap: usize,
) -> Result<(Option<i32>, String), String> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let over = Arc::new(AtomicBool::new(false));
    let (send, answer) = std::sync::mpsc::channel::<Vec<u8>>();
    let out = child.stdout.take();
    let over_out = Arc::clone(&over);
    std::thread::spawn(move || {
        let _ = send.send(read_capped(out, cap, &over_out));
    });
    let err = child.stderr.take();
    let over_err = Arc::clone(&over);
    std::thread::spawn(move || {
        let _ = read_capped(err, cap, &over_err);
    });
    let too_much = || format!("more than {cap} bytes of output");
    let started = Instant::now();
    let status = loop {
        if over.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(too_much());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} seconds", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let left = limit.saturating_sub(started.elapsed());
    let Ok(bytes) = answer.recv_timeout(left) else {
        return Err(format!(
            "the output stayed open for {} seconds",
            limit.as_secs()
        ));
    };
    if over.load(Ordering::SeqCst) {
        return Err(too_much());
    }
    Ok((status.code(), String::from_utf8_lossy(&bytes).into_owned()))
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message")
}

fn is_commit_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The null device, which stands for "no file" in git's configuration variables.
const NULL_DEVICE: &str = "/dev/null";

/// git, run apart from the user's and the system's git configuration: no `GIT_*` variable of the caller, no global or system file, no hooks, no
/// credential helper, no prompt, only the protocol of the source, every received object checked, and a stall limit.
#[derive(Clone, Debug)]
pub struct Git {
    program: OsString,
}

/// What `--check` reads at a commit without a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peek {
    pub version: String,
    pub changelog: String,
    pub tree: String,
}

/// A checked-out commit, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub dir: PathBuf,
    pub commit: String,
    pub tree: String,
    pub version: String,
}

impl Git {
    /// The `git` on `PATH`.
    pub fn system() -> Self {
        Self::with_program("git")
    }

    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn command(&self, url: &str) -> Command {
        let mut c = Command::new(&self.program);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                c.env_remove(k);
            }
        }
        let scheme = if url.to_ascii_lowercase().starts_with("file://") {
            "file"
        } else {
            "https"
        };
        c.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .args(["-c", &format!("core.hooksPath={NULL_DEVICE}")])
            .args(["-c", "credential.helper="])
            .args(["-c", "protocol.allow=never"])
            .args(["-c", &format!("protocol.{scheme}.allow=always")])
            .args(["-c", "transfer.fsckObjects=true"])
            .args([
                "-c",
                "http.lowSpeedLimit=1000",
                "-c",
                "http.lowSpeedTime=60",
            ]);
        c
    }

    /// The command of the look: [`Git::command`] closed further, because the terminal app starts it by itself. `ham update` does
    /// not use it. On top of that command:
    /// - the working directory is `/` and `GIT_DIR` is the null device, so git finds no repository, whatever folder the app was started
    ///   in, and reads no repository's own configuration (an address rewrite, a proxy, an extra header, a cookie file, a program to ask
    ///   for logins);
    /// - `GIT_ASKPASS` is set and empty, so git starts no program to ask for a login, also not the one `SSH_ASKPASS` names, and no dialog
    ///   can open; `SSH_ASKPASS` is removed as well;
    /// - `SSLKEYLOGFILE`, a debugging variable that makes the TLS library write a file, is removed;
    /// - `--heads --tags` stand before the two patterns, so git (protocol 2) asks the source for branch and tag names only.
    ///
    /// This is hardening, not a sandbox: git starts its own helper for the transport, and its libraries read the person's proxy and
    /// certificate settings and `~/.netrc`, as for every git call.
    #[doc(hidden)]
    pub fn look_command(&self, url: &str) -> Command {
        let mut c = self.command(url);
        c.current_dir("/")
            .env("GIT_DIR", NULL_DEVICE)
            .env("GIT_ASKPASS", "")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSLKEYLOGFILE")
            .args(["ls-remote", "--exit-code", "--heads", "--tags", "--"])
            .arg(url)
            .args(["refs/heads/main", "refs/tags/v*"]);
        c
    }

    /// The look of the terminal app: the newest release `url` names, from one listing call that is ended after `limit`. Exit 0 is
    /// read by `ham_core::update::newest_of`; exit 2 (nothing matched) is no release; any other end, an output over [`LOOK_OUTPUT_CAP`] and
    /// a listing that is none are a look that failed. It writes nothing, and it never returns [`LookOutcome::Off`].
    pub fn look(&self, url: &str, limit: Duration) -> LookOutcome {
        use ham_core::update::Heads;
        match run_look(self.look_command(url), limit, LOOK_OUTPUT_CAP) {
            Ok((Some(0), listing)) => match ham_core::update::newest_of(&listing) {
                Heads::Newest(version) => LookOutcome::Newest(version),
                Heads::NoRelease => LookOutcome::NoRelease,
                Heads::Garbled => LookOutcome::Failed,
            },
            Ok((Some(2), _)) => LookOutcome::NoRelease,
            Ok(_) | Err(_) => LookOutcome::Failed,
        }
    }

    fn run(&self, url: &str, dir: Option<&Path>, args: &[&str]) -> Result<Ran, UpdateError> {
        let mut c = self.command(url);
        if let Some(d) = dir {
            c.arg("-C").arg(d);
        }
        c.args(args);
        run_bounded(c, GIT_LIMIT)
            .map_err(|e| UpdateError(format!("git {}: {e}", args.first().copied().unwrap_or(""))))
    }

    fn ok(&self, url: &str, dir: Option<&Path>, args: &[&str]) -> Result<String, UpdateError> {
        let r = self.run(url, dir, args)?;
        if !r.ok {
            return fail(format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                first_line(&r.stderr)
            ));
        }
        Ok(r.stdout.trim().to_string())
    }

    /// The commit at the head of `refs/heads/main` of `url`.
    pub fn head_of_main(&self, url: &str) -> Result<String, UpdateError> {
        let r = self.run(
            url,
            None,
            &["ls-remote", "--exit-code", "--", url, "refs/heads/main"],
        )?;
        if !r.ok {
            return fail(format!(
                "could not read the main branch of {url}: {}",
                first_line(&r.stderr)
            ));
        }
        let sha = r
            .stdout
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if !is_commit_id(&sha) {
            return fail(format!("{url} answered with no commit id for main"));
        }
        Ok(sha)
    }

    /// The clone in `dir`: made with no checkout (and no file contents until they are needed) when `dir` does not exist; an existing `dir`
    /// must be a clone of `url` that holds `commit`, and is never a link.
    fn clone_of(&self, url: &str, dir: &Path, commit: &str) -> Result<(), UpdateError> {
        let shown = dir.display();
        match std::fs::symlink_metadata(dir) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return fail(format!(
                    "{shown} is not a folder ham made; move it aside and run ham update again"
                ));
            }
            Ok(_) => {
                let origin = self
                    .ok(url, Some(dir), &["config", "--get", "remote.origin.url"])
                    .unwrap_or_default();
                if origin != url {
                    return fail(format!(
                        "{shown} is not a clone of {url}; move it aside and run ham update again"
                    ));
                }
            }
            Err(_) => {
                if let Some(parent) = dir.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        UpdateError(format!("could not create {}: {e}", parent.display()))
                    })?;
                }
                let target = dir.to_string_lossy().into_owned();
                self.ok(
                    url,
                    None,
                    &[
                        "clone",
                        "--quiet",
                        "--no-checkout",
                        "--filter=blob:none",
                        "--",
                        url,
                        &target,
                    ],
                )?;
            }
        }
        let have = self.run(
            url,
            Some(dir),
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )?;
        if !have.ok {
            return fail(format!(
                "{url} does not hold commit {commit} any more (main moved); run ham update again to see the newest commit"
            ));
        }
        Ok(())
    }

    /// `VERSION`, `CHANGELOG.md` and the tree of `commit`, read from the clone in `dir` without a checkout.
    pub fn peek(&self, url: &str, dir: &Path, commit: &str) -> Result<Peek, UpdateError> {
        self.clone_of(url, dir, commit)?;
        let version = self.ok(url, Some(dir), &["show", &format!("{commit}:VERSION")])?;
        let changelog = self
            .ok(url, Some(dir), &["show", &format!("{commit}:CHANGELOG.md")])
            .unwrap_or_default();
        let tree = self.ok(
            url,
            Some(dir),
            &["rev-parse", &format!("{commit}^{{tree}}")],
        )?;
        Ok(Peek {
            version,
            changelog,
            tree,
        })
    }

    /// `commit` checked out in `dir` (detached) and checked: the head is the commit, the working tree has no change, no untracked and no
    /// ignored file, and `VERSION` is `N.NN`. A copy that fails a check is named and left as it is.
    pub fn fetch(&self, url: &str, dir: &Path, commit: &str) -> Result<Fetched, UpdateError> {
        self.clone_of(url, dir, commit)?;
        let shown = dir.display();
        let not_clean = |why: String| {
            UpdateError(format!(
                "{shown} is not a clean copy of commit {commit} ({why}); move it aside and run ham update again"
            ))
        };
        self.ok(url, Some(dir), &["checkout", "--quiet", "--detach", commit])
            .map_err(|e| not_clean(e.0))?;
        let head = self.ok(url, Some(dir), &["rev-parse", "HEAD"])?;
        if head != commit {
            return Err(not_clean(format!("its head is {head}")));
        }
        let status = self.ok(url, Some(dir), &["status", "--porcelain", "--ignored"])?;
        if !status.is_empty() {
            return Err(not_clean(format!("changed: {}", first_line(&status))));
        }
        let tree = self.ok(url, Some(dir), &["rev-parse", "HEAD^{tree}"])?;
        let version = std::fs::read_to_string(dir.join("VERSION"))
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if ham_core::update::parse_version(&version).is_none() {
            return Err(not_clean(format!("its VERSION is {version:?}")));
        }
        Ok(Fetched {
            dir: dir.to_path_buf(),
            commit: head,
            tree,
            version,
        })
    }
}

/// The folder of the clone of `commit` under the download directory.
pub fn clone_dir(downloads: &Path, commit: &str) -> PathBuf {
    downloads.join(format!("ham-update-{}", &commit[..12.min(commit.len())]))
}

/// Where the newest commit is read before an update goes ahead, so that a look leaves nothing in the download directory: `--check`
/// changes nothing, and neither does an update that is up to date, refused or not confirmed. The folder is the copy of an earlier update of
/// this commit when something is at its place (it is checked like any copy and left as it is), else a new folder
/// `.ham-update-<process>-<time>` beside it, which is removed when this value is dropped. A download directory that did not exist (a
/// setting names one that was never made) is removed again with it, level by level, as long as each is empty. A process that is interrupted
/// or killed first leaves its folder, and the next look removes it ([`sweep_looks`]).
struct Look {
    dir: PathBuf,
    /// The folder is this look's own and is removed with it.
    own: bool,
    /// The download directory and the folders above it that did not exist before this look, innermost first.
    made: Vec<PathBuf>,
}

impl Look {
    fn of(downloads: &Path, copy: &Path) -> Look {
        sweep_looks(downloads);
        if std::fs::symlink_metadata(copy).is_ok() {
            return Look {
                dir: copy.to_path_buf(),
                own: false,
                made: Vec::new(),
            };
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = downloads.join(format!("{LOOK_PREFIX}{}-{stamp}", std::process::id()));
        // A name of this process and this nanosecond: what is at it was made by this look.
        let own = std::fs::symlink_metadata(&dir).is_err();
        let made = downloads
            .ancestors()
            .take_while(|p| !p.as_os_str().is_empty() && std::fs::symlink_metadata(p).is_err())
            .map(Path::to_path_buf)
            .collect();
        Look { dir, own, made }
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    /// The update goes ahead: the folder the commit was read in becomes the copy of the commit at `copy`, so nothing is fetched twice. A
    /// folder that cannot be moved there is removed as after a check, and the copy is fetched anew.
    fn keep_as(mut self, copy: &Path) {
        if self.own
            && std::fs::symlink_metadata(copy).is_err()
            && std::fs::rename(&self.dir, copy).is_ok()
        {
            self.own = false;
        }
    }
}

impl Drop for Look {
    fn drop(&mut self) {
        if self.own {
            let _ = std::fs::remove_dir_all(&self.dir);
            // `remove_dir` takes an empty folder only: a folder that holds anything, or that was there before, stays.
            for folder in &self.made {
                if std::fs::remove_dir(folder).is_err() {
                    break;
                }
            }
        }
    }
}

/// How the name of the folder a look reads in begins: `.ham-update-<process>-<time>`.
const LOOK_PREFIX: &str = ".ham-update-";

/// The process whose look made a folder of this name; `None` for every name that is not `.ham-update-<process>-<time>` in digits.
fn look_process(name: &str) -> Option<u32> {
    let (process, time) = name.strip_prefix(LOOK_PREFIX)?.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(process) && digits(time) {
        process.parse().ok()
    } else {
        None
    }
}

/// Whether a process with this id runs on this machine; `None` when that cannot be told. Linux lists every process as a folder of `/proc`.
/// Elsewhere `ps` is asked for this process and the other one together, and an answer that does not name this process tells nothing
/// about the other.
fn process_runs(pid: u32) -> Option<bool> {
    let own = std::process::id();
    if pid == own {
        return Some(true);
    }
    let listed = Path::new("/proc");
    if listed.join(own.to_string()).is_dir() {
        return Some(listed.join(pid.to_string()).exists());
    }
    process_runs_by(OsStr::new("ps"), own, pid)
}

/// [`process_runs`] by `<ps> -o pid= -p <own>,<pid>`, which prints the ids of those of the two that run.
fn process_runs_by(ps: &OsStr, own: u32, pid: u32) -> Option<bool> {
    let mut command = Command::new(ps);
    command.args(["-o", "pid=", "-p", &format!("{own},{pid}")]);
    let ran = run_bounded(command, Duration::from_secs(10)).ok()?;
    let running: Vec<u32> = ran
        .stdout
        .split_whitespace()
        .filter_map(|id| id.parse().ok())
        .collect();
    running.contains(&own).then(|| running.contains(&pid))
}

/// Removes the folders that earlier looks left in `downloads`. A look removes its own folder when it ends, but a `ham` that is
/// interrupted or killed first (Ctrl-C while the commit is read, or at the question) cannot, and no later look uses that name again. A
/// folder goes when its name is a look's, it is a folder and no link, and the process in its name runs no more. A folder whose process
/// runs stays (another `ham update` in this download directory), and so does one whose process cannot be looked up; a process id that
/// another program has taken keeps its folder until that program ends. Nothing else in the directory is touched. The processes are
/// those this machine can see: in a download directory shared with another machine or container, a look that runs there can lose its
/// folder to a look here; it then stops with git's error and has changed nothing.
fn sweep_looks(downloads: &Path) {
    let Ok(entries) = std::fs::read_dir(downloads) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(process) = entry.file_name().to_str().and_then(look_process) else {
            continue;
        };
        // `file_type` does not follow a link, and `remove_dir_all` follows none inside the folder.
        if entry.file_type().is_ok_and(|t| t.is_dir()) && process_runs(process) == Some(false) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// How the wait goes.
#[derive(Clone, Debug)]
pub struct DrainOptions {
    pub poll: Duration,
    pub limit: Duration,
    /// Stop at once when runs are active.
    pub no_wait: bool,
}

/// How the wait ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrainEnd {
    /// No run was active.
    Clear,
    /// Runs were active and have ended.
    Waited { seconds: u64 },
    /// The limit came first: the runs still active.
    TimedOut(Vec<String>),
    /// `--no-wait` and these runs are active.
    NotWaiting(Vec<String>),
    /// Ctrl-C during the wait.
    Interrupted,
}

fn active(runs: &[RunSummary]) -> Vec<&RunSummary> {
    runs.iter()
        .filter(|r| r.status == GoalStatus::Active)
        .collect()
}

/// The line the wait prints: the runs (the first five, and how many more), the minutes left, and that no run is cancelled.
pub fn waiting_line(runs: &[&RunSummary], left: Duration) -> String {
    let shown: Vec<String> = runs
        .iter()
        .take(5)
        .map(|r| {
            if r.needs_input.is_some() {
                format!("{} (waits for your answer: ham say {})", r.run_id, r.run_id)
            } else {
                r.run_id.clone()
            }
        })
        .collect();
    let more = if runs.len() > 5 {
        format!(" and {} more", runs.len() - 5)
    } else {
        String::new()
    };
    let minutes = left.as_secs().div_ceil(60).max(1);
    let noun = if runs.len() == 1 { "run" } else { "runs" };
    format!(
        "Waiting for {} {noun} to end: {}{more}. {minutes} min left. Ctrl-C stops the wait. No run is cancelled.",
        runs.len(),
        shown.join(", ")
    )
}

/// Waits until none of the caller's runs is active, up to `opts.limit`. It reads the list of runs only: it never cancels a run.
pub async fn drain<P: ControlPlane>(
    api: &Api<P>,
    opts: &DrainOptions,
    out: &Output,
) -> Result<DrainEnd, ApiError> {
    let started = Instant::now();
    let mut waited = false;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        // Every page, each turn: an active run can stand on any page (a long run, one that waits for an answer, a resumed one).
        let runs = api.list_runs().await?;
        let now = active(&runs);
        let ids: Vec<String> = now.iter().map(|r| r.run_id.clone()).collect();
        if ids.is_empty() {
            return Ok(if waited {
                DrainEnd::Waited {
                    seconds: started.elapsed().as_secs(),
                }
            } else {
                DrainEnd::Clear
            });
        }
        if opts.no_wait {
            return Ok(DrainEnd::NotWaiting(ids));
        }
        let elapsed = started.elapsed();
        if elapsed >= opts.limit {
            return Ok(DrainEnd::TimedOut(ids));
        }
        let left = opts.limit - elapsed;
        out.err_line(waiting_line(&now, left));
        waited = true;
        tokio::select! {
            _ = tokio::time::sleep(opts.poll.min(left)) => {}
            _ = &mut interrupt => return Ok(DrainEnd::Interrupted),
        }
    }
}

/// The line that says why `ham update` did not wait for the caller's active runs: the first five, and how many more.
fn deploys_nothing_line(own: &[String]) -> String {
    let shown: Vec<&str> = own.iter().take(5).map(String::as_str).collect();
    let more = if own.len() > 5 {
        format!(" and {} more", own.len() - 5)
    } else {
        String::new()
    };
    let noun = if own.len() == 1 { "run" } else { "runs" };
    format!(
        "Setup's plan for this update deploys nothing on the stage, so ham update does not wait for your {} active {noun}: {}{more}. No run is cancelled. Setup decides again when it runs.",
        own.len(),
        shown.join(", ")
    )
}

/// The line `ham update` prints before it asks setup for its plan, so that the time the plan takes is no silence: why it asks, that
/// setup changes nothing, about how long the plan takes, and how long `ham update` waits for it at most.
fn asking_line(limit: Duration) -> String {
    format!(
        "Asking setup for its plan because a run of yours is active. Setup reads your AWS account and changes nothing; it takes about 20 seconds. ham update waits at most {} for the plan.",
        ham_core::render::wait_text(limit)
    )
}

/// What a process writes to one of its outputs until it ends. The pipe is this client's own, and the text ends at a mark that the
/// client writes into the pipe once the process has ended, never at the end of the pipe: a process that setup's calls leave behind (the
/// agent of a credential helper, for one) keeps the output it was started with open for as long as it lives, and what setup printed is
/// complete when setup has ended.
struct Captured {
    mark: Vec<u8>,
    writer: std::io::PipeWriter,
    reader: std::thread::JoinHandle<Vec<u8>>,
}

impl Captured {
    /// A pipe with a thread that reads it, and the end of the pipe that the process writes to.
    fn open(name: &str) -> std::io::Result<(Captured, Stdio)> {
        let (mut from, writer) = std::io::pipe()?;
        let theirs = writer.try_clone()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // A NUL byte on each side: no line that setup prints holds one.
        let mark = format!("\0ham-update-{name}-{}-{stamp}\0", std::process::id()).into_bytes();
        let end = mark.clone();
        let reader = std::thread::spawn(move || {
            let mut text = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match from.read(&mut chunk) {
                    Ok(0) => return text,
                    Ok(n) => {
                        // The mark can lie across two reads: look from where it could begin.
                        let from_here = text.len().saturating_sub(end.len() - 1);
                        text.extend_from_slice(&chunk[..n]);
                        let found = text[from_here..]
                            .windows(end.len())
                            .position(|w| w == end.as_slice());
                        if let Some(at) = found {
                            text.truncate(from_here + at);
                            return text;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return text,
                }
            }
        });
        Ok((
            Captured {
                mark,
                writer,
                reader,
            },
            Stdio::from(theirs),
        ))
    }

    /// What was written until now; called when the process has ended. The mark ends the read, whoever else still holds the pipe.
    fn text(self) -> String {
        let Captured {
            mark,
            mut writer,
            reader,
        } = self;
        if !reader.is_finished() {
            let _ = writer.write_all(&mark);
        }
        drop(writer);
        String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned()
    }
}

/// The plan of `<dir>/setup.sh --update` for this update, asked with `--dry-run`: setup reads the AWS account, writes nothing and
/// prints what it would do. It gets the arguments the update itself gives it, and always a wait, so that an active run does not end the plan
/// before it is printed. `Ok(true)` when the plan says that the update deploys nothing on the stage
/// (`ham_core::update::plan_deploys_nothing`), `Ok(false)` for every other plan, and `Err` with the reason when setup printed none within
/// `limit` or ended with an error. The plan is read when setup has ended ([`Captured`]): a process that setup left behind with its output
/// does not hold the plan back. Nothing of setup's output is shown: the update's own run of setup prints its plan.
async fn setup_plan(
    dir: &Path,
    prefix: &Path,
    minutes: u64,
    limit: Duration,
) -> Result<bool, String> {
    const ASKED: &str = "setup.sh --update --dry-run";
    let no_start = |e: std::io::Error| format!("{ASKED} could not start: {e}");
    let (out, to_out) = Captured::open("out").map_err(no_start)?;
    let (err, to_err) = Captured::open("err").map_err(no_start)?;
    let mut command = tokio::process::Command::new(dir.join("setup.sh"));
    command
        .arg("--update")
        .arg("--prefix")
        .arg(prefix)
        .arg("--yes")
        .arg("--wait-for-runs")
        .arg(minutes.to_string())
        .arg("--dry-run")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(to_out)
        .stderr(to_err)
        .kill_on_drop(true);
    let started = command.spawn();
    // The command holds the ends of the pipes that setup writes to: with it gone, this client holds only the ends it marks.
    drop(command);
    let mut setup = started.map_err(no_start)?;
    let ended = tokio::time::timeout(limit, setup.wait()).await;
    if ended.is_err() {
        let _ = setup.kill().await;
    }
    // Setup has ended, by itself or by the limit: what it printed is in the pipes.
    let (plan, complaint) = (out.text(), err.text());
    match ended {
        Err(_) => Err(format!(
            "{ASKED} gave no answer within {}",
            ham_core::render::wait_text(limit)
        )),
        Ok(Err(e)) => Err(format!("{ASKED} could not be followed to its end: {e}")),
        Ok(Ok(status)) if !status.success() => {
            let code = status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
            let said: String = first_line(&complaint).chars().take(200).collect();
            Err(format!(
                "{ASKED} ended with {code}: {}",
                ham_core::sanitize::sanitize_line(&said)
            ))
        }
        Ok(Ok(_)) => Ok(ham_core::update::plan_deploys_nothing(&plan)),
    }
}

/// The minutes `ham update` hands to `setup.sh --update --wait-for-runs`: what is left of `limit` after `elapsed`, rounded up, and never less than
/// one (setup takes 1 to 1440). Setup waits that long for the runs of everyone on the stage, so a person sees one limit.
pub fn minutes_left(limit: Duration, elapsed: Duration) -> u64 {
    limit.saturating_sub(elapsed).as_secs().div_ceil(60).max(1)
}

/// The SHA-256 of a file, in lower-case hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// What the installed client says about itself after the update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFit {
    Fits,
    /// The new client reported `reported`; the earlier client is back in place.
    Restored {
        reported: String,
    },
    /// The new client reported `reported`, and the earlier client could not be put back.
    NotRestored {
        reported: String,
        why: String,
    },
}

/// What a client answered to `--version`, for a sentence: `reported v9.99` for a version, else the words of [`version_of`] (`answered with an
/// error (...)`, `gave no answer (...)`), never a `v` in front of words that are no version.
fn answered(reported: &str) -> String {
    if ham_core::update::parse_version(reported).is_some() {
        format!("reported v{reported}")
    } else if let Some(why) = reported.strip_prefix("no answer") {
        format!("gave no answer{why}")
    } else {
        format!("answered with {reported}")
    }
}

/// The client's version for the result line: `vN.NN`, or that it is not known when the client answered no version.
fn version_words(reported: &str) -> String {
    if ham_core::update::parse_version(reported).is_some() {
        format!("v{reported}")
    } else {
        "of an unknown version".to_string()
    }
}

/// The version a client prints for `--version` (`ham vN.NN`), or what it printed instead.
fn version_of(bin: &Path) -> String {
    match run_bounded(
        {
            let mut c = Command::new(bin);
            c.arg("--version");
            c
        },
        Duration::from_secs(30),
    ) {
        Ok(r) if r.ok => {
            let line = first_line(&r.stdout);
            match line.strip_prefix("ham v") {
                Some(v) if ham_core::update::parse_version(v).is_some() => v.to_string(),
                _ => format!("the line {line:?}"),
            }
        }
        Ok(r) => format!("an error ({})", first_line(&r.stderr)),
        Err(e) => format!("no answer ({e})"),
    }
}

/// The installed client `bin` must report `expected`. If it does not, the earlier client that setup kept beside it (`<bin>.prev`) is renamed
/// back into place, but only when its SHA-256 is `before`, the SHA-256 of the running ham taken before setup started.
pub fn client_fit(bin: &Path, expected: &str, before: Option<&str>) -> ClientFit {
    let reported = version_of(bin);
    if reported == expected {
        return ClientFit::Fits;
    }
    let mut prev = bin.as_os_str().to_owned();
    prev.push(".prev");
    let prev = PathBuf::from(prev);
    let why = match (before, sha256_file(&prev)) {
        (Some(want), Ok(have)) if want == have => match std::fs::rename(&prev, bin) {
            Ok(()) => return ClientFit::Restored { reported },
            Err(e) => format!(
                "renaming {} to {} failed: {e}",
                prev.display(),
                bin.display()
            ),
        },
        (_, Ok(_)) => format!(
            "{} is not the client that ran before this update, so it was left as it is",
            prev.display()
        ),
        (_, Err(e)) => format!("{} cannot be read: {e}", prev.display()),
    };
    ClientFit::NotRestored { reported, why }
}

/// The backend's version after the update: read again until it is `expected` or `limit` has passed. The version is one that an answer
/// gave after the update: the client first forgets the one it remembers, which was read before setup ran. A read is made for the version
/// header alone, so it asks for one page of the run list, each turn, and a read that fails after its answer
/// carried the header (a list this client cannot decode) still gives that version.
/// `Err` with the failure of the last read when no answer gave a version and that read failed: the backend did not answer.
pub async fn backend_after<P: ControlPlane>(
    api: &Api<P>,
    expected: &str,
    limit: Duration,
    gap: Duration,
) -> Result<Option<String>, ApiError> {
    // An answer with no version header sets the remembered version to unknown.
    crate::version::note_backend(&std::collections::BTreeMap::new());
    let started = Instant::now();
    loop {
        let read = api.runs_answer().await;
        let now = crate::version::backend();
        if now.as_deref() == Some(expected) || started.elapsed() >= limit {
            return match (now, read) {
                (None, Err(e)) => Err(e),
                (now, _) => Ok(now),
            };
        }
        tokio::time::sleep(gap).await;
    }
}

/// The options of one `ham update`.
#[derive(Clone, Debug)]
pub struct Request {
    pub check: bool,
    pub yes: bool,
    pub json: bool,
    pub drain: DrainOptions,
}

/// What the command needs from where it runs.
pub struct Context<'a> {
    pub source: String,
    pub downloads: PathBuf,
    pub git: Git,
    /// The running ham, links resolved.
    pub exe: PathBuf,
    /// Standard input and output are a terminal.
    pub terminal: bool,
    /// Asks the question on the terminal and returns the answer.
    pub ask: &'a (dyn Fn(&str) -> Option<String> + Send + Sync),
    pub backend_recheck: Duration,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("source", &self.source)
            .field("downloads", &self.downloads)
            .field("exe", &self.exe)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

fn changes_json(changes: &[Change]) -> Vec<serde_json::Value> {
    changes
        .iter()
        .map(|c| serde_json::json!({"version": c.version, "heading": c.heading, "body": c.body}))
        .collect()
}

fn refusal_text(r: &Refusal, newest: &str) -> String {
    match r {
        Refusal::BackendUnknown => "the backend reports no version, so ham update cannot tell whether main is newer. Update from a clone with ./setup.sh --update.".into(),
        Refusal::Downgrade { part, runs } => {
            // A backend can run a version that main does not have without running newer code (a release label a withdrawn release or a test left behind), and a deploy of
            // main from a clone writes main's version; setup says if it needs more (a stage that runs newer code stops it with the way to go on). A client has no such way.
            let way = if *part == "the backend" {
                " To put the backend on main's version, run ./setup.sh --update --force-deploy in a clone of main."
            } else {
                ""
            };
            format!(
                "{part} runs v{runs}, newer than v{newest} on main, and ham update never moves a version back.{way}"
            )
        }
        Refusal::BadVersion(v) => format!("{v:?} is not a version of the form N.NN"),
    }
}

/// The codes with which setup ends after it deployed the backend and installed the client: 3, the client is installed and `ham doctor` found a
/// problem; 8, setup is done and only its runner could not be removed. The update itself took effect, so both versions are checked as after
/// any update, and setup's code stays the exit code unless a check fails.
pub const SETUP_DONE_CODES: [i32; 2] = [3, 8];

/// What a code of setup means, in one line after its own words.
fn setup_code_text(code: i32, dir: &Path) -> String {
    match code {
        3 => "setup installed ham, and ham doctor found a problem. Its message is above. The versions are checked below.".into(),
        8 => "setup finished the update, but could not remove its runner. Its message is above. The versions are checked below.".into(),
        4 => "setup was cancelled before it changed anything.".into(),
        6 => "setup could not finish a read or write in your AWS account, or its build is still running there. Its message above says which. Run ham update again.".into(),
        7 => format!(
            "setup stopped before the update, and its message is above. If the cause is a conflicting operation (for example runs that were still active at the end of the wait, or another deploy), run ham update again once it is gone. If the cause is a stage commit that this clone cannot be shown to be newer than, ham update fetches the same clone again and stops the same way: run the command that the message names in {}, for example ./setup.sh --update --revision <commit>.",
            dir.display()
        ),
        _ => "its message is above.".into(),
    }
}

/// `ham update`: returns the exit code.
pub async fn run<P: ControlPlane>(
    api: &Api<P>,
    req: &Request,
    ctx: &Context<'_>,
    out: &Output,
) -> i32 {
    run_with(api, req, ctx, out, PLAN_LIMIT).await
}

/// [`run`] with the time setup may take to print its plan ([`PLAN_LIMIT`]).
pub async fn run_with<P: ControlPlane>(
    api: &Api<P>,
    req: &Request,
    ctx: &Context<'_>,
    out: &Output,
    plan_limit: Duration,
) -> i32 {
    let client = crate::version::CLIENT;
    // 1. The head of main, and the version and the changes at it.
    let commit = match ctx.git.head_of_main(&ctx.source) {
        Ok(c) => c,
        Err(e) => {
            out.err_line(format!("ham update: {e}"));
            return exit::FAILED;
        }
    };
    let dir = clone_dir(&ctx.downloads, &commit);
    // Read in a folder that is removed again unless the update goes ahead, so a look leaves nothing in the download directory.
    let look = Look::of(&ctx.downloads, &dir);
    let peek = match ctx.git.peek(&ctx.source, look.dir(), &commit) {
        Ok(p) => p,
        Err(e) => {
            out.err_line(format!("ham update: {e}"));
            return exit::FAILED;
        }
    };
    let newest = peek.version.clone();
    // 2. The backend's version: the header of one answer of the run list carries it. The wait reads the caller's runs itself.
    if let Err(e) = api.runs_answer().await {
        out.err_line(format!("ham update: could not reach the backend: {e}"));
        return exit::FAILED;
    }
    let backend = crate::version::backend();
    let the_plan = plan(client, backend.as_deref(), &newest);
    let lower = match backend.as_deref() {
        Some(b) if ham_core::update::parse_version(b) < ham_core::update::parse_version(client) => {
            b
        }
        _ => client,
    };
    let changes = changes_between(&peek.changelog, lower, &newest);
    if req.check {
        return check_report(
            req,
            ctx,
            out,
            &commit,
            &peek,
            backend.as_deref(),
            &the_plan,
            &changes,
        );
    }
    let parts = match the_plan {
        Plan::UpToDate => {
            out.out_line(format!(
                "ham v{client} | backend v{newest}: up to date with main ({}).",
                &commit[..12]
            ));
            return exit::DONE;
        }
        Plan::Refuse(r) => {
            out.err_line(format!("ham update: {}", refusal_text(&r, &newest)));
            out.err_line("No deployment or client change was made.");
            return exit::REFUSED;
        }
        Plan::Update(p) => p,
    };
    // 3. Where the client is installed: <prefix>/bin/ham, the layout setup installs.
    let bin_dir = ctx.exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let prefix = match (bin_dir.file_name(), bin_dir.parent()) {
        (Some(n), Some(p)) if n == "bin" => p.to_path_buf(),
        _ => {
            out.err_line(format!(
                "ham update: this ham runs from {}, and setup installs the client into <prefix>/bin. Run ./setup.sh --update --prefix <folder> from a clone instead.",
                bin_dir.display()
            ));
            return exit::FAILED;
        }
    };
    // 4. The plan and the one question.
    out.out_line(format!(
        "Update to v{newest} ({}): ham v{client} | backend v{}.",
        parts.words(),
        backend.as_deref().unwrap_or("?.??")
    ));
    out.out_line(format!("  source:  {}", ctx.source));
    out.out_line(format!("  commit:  {commit} (tree {})", peek.tree));
    for c in &changes {
        out.out_line(format!("  {}", c.heading));
        for line in c.body.lines().filter(|l| l.starts_with("- ")) {
            out.out_line(format!("    {line}"));
        }
    }
    out.out_line(format!(
        "  how:     run the setup.sh --update of this commit from {}: it deploys the backend in the AWS account of your profile when this release changes it, then installs ham in {}/bin.",
        dir.display(),
        prefix.display()
    ));
    // Whether the update deploys is setup's answer, which comes after the question, so the line says what each answer means.
    if req.drain.no_wait {
        out.out_line("  wait:    none (--no-wait): an update that deploys stops when a run is active, yours or anyone's. An update that deploys nothing on the stage goes ahead. No run is cancelled.");
    } else {
        out.out_line(format!(
            "  wait:    an update that deploys waits until your active runs end, and setup then waits for the runs of everyone on the stage: at most {} min for both waits together. While setup waits, new runs are refused. An update that deploys nothing on the stage waits for no run. No run is cancelled.",
            req.drain.limit.as_secs().div_ceil(60)
        ));
    }
    if !req.yes {
        if !ctx.terminal {
            out.err_line("ham update: there is no terminal to ask on. Add --yes to update without the question. Nothing was changed.");
            return exit::CANCELLED;
        }
        let answer = (ctx.ask)(&format!("Update to v{newest}? [y/N] "));
        if !matches!(
            answer.as_deref().map(str::trim),
            Some("y" | "Y" | "yes" | "Yes")
        ) {
            out.err_line("Nothing was changed.");
            return exit::CANCELLED;
        }
    }
    // 5. The commit itself, checked out and checked, in the folder it was read in.
    look.keep_as(&dir);
    let fetched = match ctx.git.fetch(&ctx.source, &dir, &commit) {
        Ok(f) => f,
        Err(e) => {
            out.err_line(format!("ham update: {e}"));
            return exit::REFUSED;
        }
    };
    if fetched.version != newest {
        out.err_line(format!(
            "ham update: the checked-out VERSION is {}, not {newest}; nothing was changed.",
            fetched.version
        ));
        return exit::REFUSED;
    }
    if newest_heading(&peek.changelog).is_some_and(|h| h != newest) {
        out.err_line(format!(
            "ham update: note: the newest heading of CHANGELOG.md at this commit is not {newest}."
        ));
    }
    // 6. The wait for the caller's runs. An update that deploys nothing on the stage waits for none: setup itself waits for no run then,
    // and the stage keeps starting runs. Whether an update deploys is setup's decision (the stage's record, the account file, the settings,
    // the stacks), so when a run of the caller is active, ham asks the setup of the fetched commit for its plan. Every other answer keeps the
    // wait. Setup decides again when it runs: if it deploys after all, it waits for the runs of everyone itself, or stops when it was given
    // no wait.
    let wait_started = Instant::now();
    // Every page: that no run of the caller is active can only be said after the last page.
    let own: Vec<String> = match api.list_runs().await {
        Ok(runs) => active(&runs).iter().map(|r| r.run_id.clone()).collect(),
        Err(e) => {
            out.err_line(format!("ham update: could not read your runs: {e}"));
            return exit::FAILED;
        }
    };
    let mut wait = !own.is_empty();
    if wait {
        let asked = minutes_left(req.drain.limit, Duration::ZERO);
        // The plan is a preflight of the AWS account: the time it takes is said before it is asked, also under `--no-wait`.
        out.err_line(asking_line(plan_limit));
        match setup_plan(&fetched.dir, &prefix, asked, plan_limit).await {
            Ok(true) => {
                wait = false;
                out.err_line(deploys_nothing_line(&own));
            }
            Ok(false) => {}
            Err(why) => out.err_line(format!(
                "ham update: could not read the plan of setup ({why}), so it waits for your runs as before an update that deploys."
            )),
        }
    }
    if wait {
        // One limit covers the plan and both waits: the time the plan took is gone from the wait for the caller's runs.
        let left = DrainOptions {
            limit: req.drain.limit.saturating_sub(wait_started.elapsed()),
            ..req.drain.clone()
        };
        match drain(api, &left, out).await {
            Ok(DrainEnd::Clear) => {}
            Ok(DrainEnd::Waited { seconds }) => out.err_line(format!(
                "Your runs have ended (waited {} min).",
                seconds.div_ceil(60)
            )),
            Ok(DrainEnd::TimedOut(ids)) => {
                out.err_line(format!(
                    "ham update: {} run(s) still active after {} min: {}. No deployment or client change was made. No run was cancelled. Run ham update again later, or give --wait <minutes>.",
                    ids.len(),
                    req.drain.limit.as_secs().div_ceil(60),
                    ids.join(", ")
                ));
                return exit::REFUSED;
            }
            Ok(DrainEnd::NotWaiting(ids)) => {
                out.err_line(format!(
                    "ham update: {} run(s) are active: {}. --no-wait stops here. No deployment or client change was made.",
                    ids.len(),
                    ids.join(", ")
                ));
                return exit::REFUSED;
            }
            Ok(DrainEnd::Interrupted) => {
                out.err_line("ham update: the wait was stopped. No deployment or client change was made. No run was cancelled.");
                return exit::CANCELLED;
            }
            Err(e) => {
                out.err_line(format!("ham update: could not read your runs: {e}"));
                return exit::FAILED;
            }
        }
    }
    // 7. setup.sh --update of the fetched commit: the backend first, then the client.
    let before = sha256_file(&ctx.exe).ok();
    // Setup takes the stage's deploy lock, so the stage starts no new runs, and waits for the runs of everyone on the stage with what is left of
    // this limit. With --no-wait it gets no wait and stops at once when anyone's run is active.
    let wait_for_runs =
        (!req.drain.no_wait).then(|| minutes_left(req.drain.limit, wait_started.elapsed()));
    let code = run_setup(&fetched.dir, &prefix, wait_for_runs, out).await;
    if code != 0 {
        out.err_line(format!(
            "ham update: setup.sh --update ended with exit {code}: {}",
            setup_code_text(code, &fetched.dir)
        ));
        // Any other code means the update did not take effect or is not over: it is passed through, and nothing is checked.
        if !SETUP_DONE_CODES.contains(&code) {
            return code;
        }
    }
    // 8. Both versions afterwards. The exit code is setup's (0, or a code of `SETUP_DONE_CODES`), unless a check fails.
    let mut fits = true;
    let installed = prefix
        .join("bin")
        .join(ctx.exe.file_name().unwrap_or_default());
    let client_now = match client_fit(&installed, &newest, before.as_deref()) {
        ClientFit::Fits => newest.clone(),
        ClientFit::Restored { reported } => {
            out.err_line(format!(
                "ham update: the new ham {}, not v{newest}, so the earlier ham (v{client}) is restored at {}. The backend was not rolled back.",
                answered(&reported),
                installed.display()
            ));
            fits = false;
            client.to_string()
        }
        ClientFit::NotRestored { reported, why } => {
            out.err_line(format!(
                "ham update: the new ham at {} {}, not v{newest}; {why}.",
                installed.display(),
                answered(&reported)
            ));
            fits = false;
            reported
        }
    };
    let read = backend_after(api, &newest, ctx.backend_recheck, Duration::from_secs(10)).await;
    let backend_now = match read {
        Ok(now) => {
            if now.as_deref() != Some(newest.as_str()) {
                out.err_line(format!(
                    "ham update: the backend reports v{} after the update, not v{newest}.",
                    now.as_deref().unwrap_or("?.??")
                ));
                // After a setup that ended without an error the stage was deployed or given the release's label, and the backend reads the label every 30 seconds,
                // so a version that is still older after the wait is a state setup did not repair. After setup's 3 or 8 nothing is known about the stage beyond what
                // setup said. Either way setup can deploy the commit again; the command carries the prefix this update gave setup, so no second client is installed.
                if backend_is_older(now.as_deref(), &newest) {
                    let cause = if code == 0 {
                        format!(
                            "setup ended without an error, so the backend should report v{newest}, but it did not within the {} s that ham waited",
                            ctx.backend_recheck.as_secs()
                        )
                    } else {
                        format!(
                            "setup reported exit {code} (its message is above), so the backend may not have taken the release"
                        )
                    };
                    out.err_line(format!(
                        "ham update: {cause}. Run ham doctor to look again. If it still reports an older version, run ./setup.sh --update --force-deploy --prefix {} in {}.",
                        prefix.display(),
                        fetched.dir.display()
                    ));
                }
                fits = false;
            }
            now
        }
        // No answer since setup ran gave a version and the last read failed: the lines give the backend none.
        Err(e) => {
            out.err_line(format!(
                "ham update: the backend did not answer after the update, so its version is not known: {e}"
            ));
            fits = false;
            None
        }
    };
    if fits {
        out.out_line(format!("Updated: ham v{newest} | backend v{newest}."));
        code
    } else {
        out.out_line(format!(
            "After the update: ham {} | backend v{}.",
            version_words(&client_now),
            backend_now.as_deref().unwrap_or("?.??")
        ));
        exit::CHECK_FAILED
    }
}

/// Whether the backend's version is a version older than `newest` (a backend that reports none, or one that is newer, is another state).
fn backend_is_older(backend: Option<&str>, newest: &str) -> bool {
    match (
        backend.and_then(ham_core::update::parse_version),
        ham_core::update::parse_version(newest),
    ) {
        (Some(b), Some(n)) => b < n,
        _ => false,
    }
}

/// Runs `<dir>/setup.sh --update --prefix <prefix> --yes` in `dir` with the terminal handed over, and with `--wait-for-runs <minutes>` when
/// `wait_for_runs` names the minutes setup may wait for the runs of everyone on the stage. Ctrl-C reaches setup, which handles it; ham waits for
/// setup to end and returns its exit code.
async fn run_setup(dir: &Path, prefix: &Path, wait_for_runs: Option<u64>, out: &Output) -> i32 {
    let mut command = tokio::process::Command::new(dir.join("setup.sh"));
    command
        .arg("--update")
        .arg("--prefix")
        .arg(prefix)
        .arg("--yes");
    if let Some(minutes) = wait_for_runs {
        command.arg("--wait-for-runs").arg(minutes.to_string());
    }
    let mut child = match command.current_dir(dir).spawn() {
        Ok(c) => c,
        Err(e) => {
            out.err_line(format!(
                "ham update: could not start {}/setup.sh: {e}",
                dir.display()
            ));
            return exit::FAILED;
        }
    };
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        tokio::select! {
            status = child.wait() => {
                return match status {
                    Ok(s) => s.code().unwrap_or(exit::FAILED),
                    Err(e) => {
                        out.err_line(format!("ham update: waiting for setup: {e}"));
                        exit::FAILED
                    }
                };
            }
            _ = &mut interrupt => {
                out.err_line("ham update: Ctrl-C went to setup; ham waits for setup to end.");
                interrupt.set(tokio::signal::ctrl_c());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_report(
    req: &Request,
    ctx: &Context<'_>,
    out: &Output,
    commit: &str,
    peek: &Peek,
    backend: Option<&str>,
    the_plan: &Plan,
    changes: &[Change],
) -> i32 {
    let client = crate::version::CLIENT;
    let newest = &peek.version;
    let available = matches!(the_plan, Plan::Update(_));
    if req.json {
        out.out_line(serde_json::json!({
            "client": client,
            "backend": backend,
            "newest": newest,
            "commit": commit,
            "tree": peek.tree,
            "source": ctx.source,
            "updateAvailable": available,
            "changes": changes_json(changes),
        }));
        return exit::DONE;
    }
    out.out_line(format!(
        "Installed: ham v{client} | backend v{}",
        backend.unwrap_or("?.??")
    ));
    out.out_line(format!(
        "Newest:    v{newest} (commit {}, main of {})",
        &commit[..12.min(commit.len())],
        ctx.source
    ));
    if !changes.is_empty() {
        out.out_line("Changes:");
        for c in changes {
            out.out_line(format!("  {}", c.heading));
            for line in c.body.lines().filter(|l| !l.trim().is_empty()) {
                out.out_line(format!("    {line}"));
            }
        }
    }
    match the_plan {
        Plan::UpToDate => out.out_line("Up to date."),
        Plan::Update(p) => out.out_line(format!(
            "Run ham update to install v{newest} ({}): the backend first, then ham. An update that deploys waits for your active runs to end first.",
            p.words()
        )),
        Plan::Refuse(r) => out.out_line(format!("ham update would stop: {}", refusal_text(r, newest))),
    }
    exit::DONE
}

/// The options a parsed `ham update` gives.
pub fn request(
    check: bool,
    yes: bool,
    json: bool,
    no_wait: bool,
    wait_minutes: Option<u32>,
) -> Request {
    Request {
        check,
        yes,
        json,
        drain: DrainOptions {
            poll: POLL,
            limit: Duration::from_secs(u64::from(wait_minutes.unwrap_or(WAIT_MINUTES)) * 60),
            no_wait,
        },
    }
}

/// The source a config names, or the reason it names none.
pub fn source_of(cfg: &ham_core::config::Config) -> Result<ham_core::update::Source, String> {
    ham_core::update::source(cfg.update_source.as_deref())
}

/// What the off switch says.
pub fn off_text(config_path: &str) -> String {
    format!(
        "ham update is turned off: update_source is off in {config_path}. Nothing was fetched. To turn it on: ham config unset update_source"
    )
}

/// Why `ham update` cannot take the backends of this config, in one sentence; `None` when it can. Setup updates one stage, and it reads and
/// writes the top-level keys of the file (`api_endpoint`, `aws_profile`, `region`):
/// - more than one backend (a top-level address and a `[[backend]]` table, or several tables) stops the update and `--check`, which reads one
///   backend's version;
/// - one backend written as a `[[backend]]` table with no top-level address is one backend: `--check` (`check` true) reads it like any single
///   stage, and the update itself stops, because setup would not find the stage in the table and would write a second backend beside it.
pub fn backend_refusal(cfg: &ham_core::config::Config, check: bool) -> Option<String> {
    let tables = cfg.backend.as_ref().map_or(0, Vec::len);
    match (ham_core::backends::backend_defs(cfg).len(), tables) {
        (2.., _) => Some(
            "config.toml names more than one backend, and setup updates the stage in the account of one profile. Run ./setup.sh --update from a clone for each account."
                .to_string(),
        ),
        (_, 1..) if !check => Some(
            "config.toml names its backend in a [[backend]] table, and setup reads and writes api_endpoint, aws_profile and region at the top level of the file. Write the backend's three keys at the top level instead of in the table, then run ham update again. ham update --check works as it is."
                .to_string(),
        ),
        _ => None,
    }
}

/// What `config.toml` and the environment say about a look of the terminal app. It reads the file and starts no process and
/// makes no call.
/// - `Ok(Some(source))`: a look may read this source.
/// - `Ok(None)`: there is to be no look. The notice is switched off (`HAMH_NO_UPDATE_NOTICE`, whatever the file holds, or
///   `ui.update_notice`), `update_source` is `off` or a value that is no source, or the config is one `ham update` would refuse (more than
///   one backend, or its backend in a `[[backend]]` table), because the notice would promise what the update cannot do.
/// - `Err(why)`: the config could not be read at this moment: no config location, a file that cannot be read, or a file with an error,
///   as an editor leaves it for a moment. That says nothing about what the person chose, so it is no switch: the terminal app takes it
///   as a look that failed, keeps what the last look found and looks again in an hour.
pub fn look_source(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, String> {
    if ham_core::config::update_notice_forced_off(env) {
        return Ok(None);
    }
    let (cfg, _) = crate::config_cmd::load(env)?;
    if !ham_core::config::update_notice_on(env, &cfg) {
        return Ok(None);
    }
    let Ok(ham_core::update::Source::Url(url)) = source_of(&cfg) else {
        return Ok(None);
    };
    if backend_refusal(&cfg, false).is_some() {
        return Ok(None);
    }
    Ok(Some(url))
}

/// The two places of the developer tools' git on a Mac. Where neither exists, `/usr/bin/git` is only Apple's launcher, which shows the
/// system's dialog that offers to install the tools.
const MAC_TOOLS_GIT: [&str; 2] = [
    "/Library/Developer/CommandLineTools/usr/bin/git",
    "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
];

/// The git a look of the terminal app may start, read once when the app starts, or `None` for no look. It starts no process:
/// `is_file` answers for a path.
/// 1. `exe` is the running client. Without it, or when the folder that holds it is not named `bin`, there is no look: `ham update` could
///    not install there, so a notice would promise what cannot be done.
/// 2. The program is the file `git` of the first folder of `path` (split as `PATH`) that holds one. A folder that is not an absolute path
///    is passed over, so the look never starts a program from the folder the app stands in. No git: no look.
/// 3. With `mac`, when that program is `/usr/bin/git` and neither path of the developer tools exists: no look, because starting the
///    launcher would open the system's install dialog at every start of the app.
pub fn look_program(
    exe: Option<&Path>,
    path: Option<&OsStr>,
    is_file: &dyn Fn(&Path) -> bool,
    mac: bool,
) -> Option<PathBuf> {
    let bin = exe?.parent()?;
    if bin.file_name() != Some(OsStr::new("bin")) {
        return None;
    }
    let git = std::env::split_paths(path?)
        .filter(|folder| folder.is_absolute())
        .map(|folder| folder.join("git"))
        .find(|candidate| is_file(candidate))?;
    if mac
        && git == Path::new("/usr/bin/git")
        && !MAC_TOOLS_GIT.iter().any(|tools| is_file(Path::new(tools)))
    {
        return None;
    }
    Some(git)
}

/// The running ham, links resolved: the client the update replaces. The path names the install folder setup is given and the file whose SHA-256 a
/// kept `ham.prev` must match before it is renamed back; it grants no access and decides nothing about who may run what.
pub fn running_exe() -> Option<PathBuf> {
    // nosemgrep: current-exe -- the path of the client being updated, used to name its install folder and to recognise its own earlier copy; not a trust or access decision
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Only the name of a look's folder names a process: every other name stays out of a sweep.
    #[test]
    fn b843_only_the_name_of_a_look_names_a_process() {
        assert_eq!(
            look_process(".ham-update-5262-1794653542885024410"),
            Some(5262)
        );
        for other in [
            ".ham-update-5262",
            ".ham-update-5262-",
            ".ham-update--1",
            ".ham-update-x-1",
            ".ham-update-1-x",
            ".ham-update-1-2-3",
            ".ham-update-+1-2",
            ".ham-update-99999999999-1",
            "ham-update-1-2",
            ".ham-update-0123456789ab",
        ] {
            assert_eq!(look_process(other), None, "{other}");
        }
    }

    /// Where `/proc` does not list the processes, `ps` tells a process that runs from one that is gone. A program that names no process,
    /// or that cannot be started, tells nothing, and nothing is removed on its word.
    #[test]
    fn b843_ps_tells_a_process_that_runs_from_one_that_is_gone() {
        let own = std::process::id();
        let ps = OsStr::new("ps");
        let mut ended = Command::new("true").spawn().unwrap();
        let gone = ended.id();
        ended.wait().unwrap();
        let mut stays = Command::new("sleep").arg("60").spawn().unwrap();
        let runs = process_runs_by(ps, own, stays.id());
        let _ = stays.kill();
        let _ = stays.wait();
        assert_eq!(runs, Some(true));
        assert_eq!(process_runs_by(ps, own, gone), Some(false));
        assert_eq!(process_runs_by(OsStr::new("true"), own, gone), None);
        assert_eq!(process_runs_by(OsStr::new("/no/such/ps"), own, gone), None);
        // The way this machine is asked.
        assert_eq!(process_runs(own), Some(true));
        assert_eq!(process_runs(gone), Some(false));
    }

    /// Setup ends with 6 before any build too (a read or a write of the deploy lock that could not finish), so the sentence does not
    /// say that an update is running, and it names no setup command, which would install into the default folder.
    #[test]
    fn b804_the_sentence_for_setup_code_6_covers_an_end_before_the_build() {
        let said = setup_code_text(6, Path::new("/clone"));
        assert!(!said.contains("the update is still running"), "{said}");
        assert!(!said.contains("./setup.sh --update"), "{said}");
        assert!(
            said.contains("could not finish a read or write in your AWS account"),
            "{said}"
        );
        assert!(said.ends_with("Run ham update again."), "{said}");
    }
}
