//! `ham update`: the newest version on the main branch of the update source, the changes between, and the update of the backend and
//! then the client through the `setup.sh --update` of the commit ham fetched and checked, after the caller's active runs have ended.
//!
//! The rules that need no process live in `ham_core::update`. This module runs git (isolated from the user's and the system's git
//! configuration), waits for runs (it never cancels one and sends no request that changes one), runs setup with the terminal handed over, and
//! checks both versions afterwards. Its only writes are the clone in the download directory and, when the new client reports another version,
//! the rename of the earlier client that setup kept beside it (`ham.prev`) back into place, and only when that file has the SHA-256 the running
//! ham had before setup started.

use crate::api::{Api, ApiError};
use crate::http::ControlPlane;
use crate::output::Output;
use ham_core::update::{Change, Plan, Refusal, changes_between, newest_heading, plan};
use harness_protocol::api::v1::RunSummary;
use harness_protocol::goal::GoalStatus;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::Read;
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
/// header alone, so a read that fails after its answer carried the header (a list this client cannot decode) still gives that version.
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
        let read = api.list_runs().await;
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
        Refusal::Downgrade { part, runs } => format!(
            "{part} runs v{runs}, newer than v{newest} on main, and ham update never moves a version back."
        ),
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
        6 => format!(
            "the update is still running in your AWS account. To follow it again: cd {} && ./setup.sh --update",
            dir.display()
        ),
        7 => "setup stopped before the update: a conflicting operation, for example runs of other people that are still active, another deploy or a source it refuses. Its message is above. Run ham update again when the cause is gone.".into(),
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
    let peek = match ctx.git.peek(&ctx.source, &dir, &commit) {
        Ok(p) => p,
        Err(e) => {
            out.err_line(format!("ham update: {e}"));
            return exit::FAILED;
        }
    };
    let newest = peek.version.clone();
    // 2. The backend's version (and the caller's runs, read again by the wait).
    if let Err(e) = api.list_runs().await {
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
        "  how:     wait until your active runs end (at most {} min), then run the setup.sh --update of this commit from {}: it deploys the backend in the AWS account of your profile, then installs ham in {}/bin. No run is cancelled.",
        req.drain.limit.as_secs().div_ceil(60),
        dir.display(),
        prefix.display()
    ));
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
    // 5. The commit itself, checked out and checked.
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
    // 6. The wait for the caller's runs.
    match drain(api, &req.drain, out).await {
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
    // 7. setup.sh --update of the fetched commit: the backend first, then the client.
    let before = sha256_file(&ctx.exe).ok();
    let code = run_setup(&fetched.dir, &prefix, out).await;
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

/// Runs `<dir>/setup.sh --update --prefix <prefix> --yes` in `dir` with the terminal handed over. Ctrl-C reaches setup, which handles it;
/// ham waits for setup to end and returns its exit code.
async fn run_setup(dir: &Path, prefix: &Path, out: &Output) -> i32 {
    let mut child = match tokio::process::Command::new(dir.join("setup.sh"))
        .arg("--update")
        .arg("--prefix")
        .arg(prefix)
        .arg("--yes")
        .current_dir(dir)
        .spawn()
    {
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
            "Run ham update to install v{newest} ({}): the backend first, then ham, after your active runs end.",
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

/// The running ham, links resolved: the client the update replaces. The path names the install folder setup is given and the file whose SHA-256 a
/// kept `ham.prev` must match before it is renamed back; it grants no access and decides nothing about who may run what.
pub fn running_exe() -> Option<PathBuf> {
    // nosemgrep: current-exe -- the path of the client being updated, used to name its install folder and to recognise its own earlier copy; not a trust or access decision
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
}
