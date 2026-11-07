//! The worktree: restored from a local path or a git bundle
//! into `<root>/<run>/<agent>/repo` on branch `harness/<run>/<agent>`; the profiles' hidden paths
//! are moved aside for the turn; the turn is committed and bundled.

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fchmod, fstat, openat, statat};
use rustix::io::Errno;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// `<workspace root>/<run>/<agent>`: the repo, hidden-path stash and turn bundles.
    pub root: PathBuf,
    pub repo: PathBuf,
    pub branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WsError {
    #[error("workspace I/O: {0}")]
    Io(String),
    #[error("git: {0}")]
    Git(String),
    #[error("hidden path {0:?} must be a relative path inside the repository")]
    Hidden(String),
    /// The checkout is no longer the repository the run started with: `.git` moved or redirected, or a settings file that does not read as it
    /// did. No git call of agentd's runs in it ([`check_git_dir`], [`GitState`]).
    #[error("the repository is not the one the run started with: {0}")]
    Untrusted(String),
}

fn io(e: std::io::Error) -> WsError {
    WsError::Io(e.to_string())
}

/// Files a test run generates that are never a role's work.
/// Listed in every
/// worktree's `.git/info/exclude`, so the turn commit never tracks them. A file the repository already tracks stays tracked.
pub const GENERATED: &[&str] = &["__pycache__/", "*.py[cod]"];

/// The build folder of cargo (and of Maven and Gradle): a Rust run in a repository with no.gitignore would otherwise commit gigabytes.
/// It is listed beside [`GENERATED`] unless the user turned the default off.
pub const BUILD_OUTPUT: &str = "target/";

/// Appends `entry` to the repository's `.git/info/exclude` unless a line already equals it; a
/// directory without `.git/info` is left alone.
pub fn append_exclude(repo: &Path, entry: &str) -> std::io::Result<()> {
    let info = repo.join(".git/info");
    if !info.is_dir() {
        return Ok(());
    }
    let path = info.join("exclude");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|l| l == entry) {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if !current.is_empty() && !current.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "{entry}")
}

/// Lists [`GENERATED`] and [`BUILD_OUTPUT`] in the worktree's `.git/info/exclude` (creating `.git/info` when the clone
/// has none), one line each however often it runs.
pub fn exclude_generated(wt: &Worktree) -> Result<(), WsError> {
    exclude_generated_with(wt, true)
}

/// [`exclude_generated`], with the build folder `target/` listed only when `build_output` is true: `false`
/// leaves it to the repository's own ignore files. Run setup calls it before the engine starts.
pub fn exclude_generated_with(wt: &Worktree, build_output: bool) -> Result<(), WsError> {
    std::fs::create_dir_all(wt.repo.join(".git/info")).map_err(io)?;
    for entry in GENERATED {
        append_exclude(&wt.repo, entry).map_err(io)?;
    }
    if build_output {
        append_exclude(&wt.repo, BUILD_OUTPUT).map_err(io)?;
    }
    Ok(())
}

fn layout(workspace_root: &Path, run: &str, agent: &str) -> Worktree {
    let root = workspace_root.join(run).join(agent);
    Worktree {
        repo: root.join("repo"),
        branch: format!("harness/{run}/{agent}"),
        root,
    }
}

/// Settings on every agentd git call: the engine may have written hooks or a hooksPath, an
/// fsmonitor command or system and global config, none of which may run when agentd commits.
/// `core.fileMode=true` whatever `.git/config` says: the turn commit records the exec bit a file has.
/// No signing program (`commit.gpgSign`), no global attributes file and no `ext::` transport is used either, so a setting for one of them in a
/// configuration file that slipped past [`GitState`] has nothing to start.
const GIT_GUARDS: &[&str] = &[
    "-c",
    "safe.directory=*",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.fileMode=true",
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "protocol.ext.allow=never",
];

/// The variables of agentd's own environment that a git call keeps: where to find the programs git starts, the language and time zone of its
/// messages, and the temp folder it was given. Nothing else of agentd's environment reaches a git call, so no `AWS_*` or `AGENTD_*` value,
/// credential variable or `GIT_*` setting of the process can be read by a program a repository setting starts.
fn passes_to_git(name: &str) -> bool {
    matches!(name, "PATH" | "LANG" | "LANGUAGE" | "TZ" | "TMPDIR") || name.starts_with("LC_")
}

/// A git call in `dir` with agentd's guards and a cleaned environment: no system or global configuration, no prompt, and only the variables of
/// [`passes_to_git`]. The caller adds the author and, for a private repository, `GIT_DIR` and the like.
fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(GIT_GUARDS).args(args).env_clear();
    for (name, value) in std::env::vars_os() {
        if name.to_str().is_some_and(passes_to_git) {
            cmd.env(name, value);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir);
    cmd
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, WsError> {
    git_bytes(dir, args)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

/// Who the engine's work is committed as.
pub const AGENT_AUTHOR: (&str, &str) = ("ham agent", "agent@harness.invalid");

/// Who the harness's own commits are made as, so a
/// reader of the history can tell them from the engine's.
pub const HARNESS_AUTHOR: (&str, &str) = ("ham harness", "harness@harness.invalid");

async fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, WsError> {
    git_bytes_as(dir, args, AGENT_AUTHOR).await
}

async fn git_bytes_as(dir: &Path, args: &[&str], who: (&str, &str)) -> Result<Vec<u8>, WsError> {
    let out = git_command(dir, args)
        .env("GIT_AUTHOR_NAME", who.0)
        .env("GIT_AUTHOR_EMAIL", who.1)
        .env("GIT_COMMITTER_NAME", who.0)
        .env("GIT_COMMITTER_EMAIL", who.1)
        .output()
        .await
        .map_err(|e| WsError::Git(format!("git {}: {e}", args.first().unwrap_or(&""))))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(WsError::Git(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            err.trim()
        )));
    }
    Ok(out.stdout)
}

/// Runs git in `dir` with agentd's guards, the harness as author, and `envs` added: the snapshot repository's `GIT_DIR`,
/// `GIT_WORK_TREE` and `GIT_INDEX_FILE` (`direct::snapshot`). The output is trimmed.
pub(crate) async fn git_with_env(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &Path)],
) -> Result<String, WsError> {
    let mut cmd = git_command(dir, args);
    cmd.env("GIT_AUTHOR_NAME", HARNESS_AUTHOR.0)
        .env("GIT_AUTHOR_EMAIL", HARNESS_AUTHOR.1)
        .env("GIT_COMMITTER_NAME", HARNESS_AUTHOR.0)
        .env("GIT_COMMITTER_EMAIL", HARNESS_AUTHOR.1)
        // An aborted snapshot must not leave its git behind.
        .kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| WsError::Git(format!("git {}: {e}", args.first().unwrap_or(&""))))?;
    if !out.status.success() {
        return Err(WsError::Git(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One git call of agentd's own plumbing (the DOX starter commit, `direct::dox`): the guards and the cleaned environment of every agentd git call, the
/// harness as author and committer, `vars` (text values such as a commit date) and, when given, a private index file in place of the repository's.
/// The whole output comes back, the exit status included: the caller decides what a non-zero exit means.
pub(crate) async fn git_plumbing(
    dir: &Path,
    args: &[&str],
    vars: &[(&str, &str)],
    index: Option<&Path>,
) -> Result<std::process::Output, WsError> {
    let mut cmd = git_command(dir, args);
    cmd.env("GIT_AUTHOR_NAME", HARNESS_AUTHOR.0)
        .env("GIT_AUTHOR_EMAIL", HARNESS_AUTHOR.1)
        .env("GIT_COMMITTER_NAME", HARNESS_AUTHOR.0)
        .env("GIT_COMMITTER_EMAIL", HARNESS_AUTHOR.1)
        .kill_on_drop(true);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.output()
        .await
        .map_err(|e| WsError::Git(format!("git {}: {e}", args.first().unwrap_or(&""))))
}

/// Names that make git read its configuration, objects or refs from another directory when they sit inside `.git`. A linked worktree has them
/// in its own folder under `.git/worktrees/`, never in `.git` itself, so a clone's `.git` has none of them.
const REDIRECTS: &[&str] = &["commondir", "gitdir"];

/// What `<repo>/.git` is, said as the reason it cannot be trusted; `None` when it is the repository's own plain directory with no redirect in it.
///
/// The engine owns the checkout for the whole run. Git reads its configuration from wherever `.git` points: `.git` can be replaced by a file
/// that names a directory of the engine's (`gitdir: <path>`), and a `commondir` file inside `.git` names another one. In both the configuration
/// is the engine's, and a clean filter it defines in it is a program agentd's `git add -A` starts, outside the engine's sandbox and with
/// agentd's rights. [`GitState`] cannot help there: the files it puts back are not the ones git reads.
pub fn git_dir_problem(repo: &Path) -> Option<String> {
    let git_dir = repo.join(".git");
    let meta = match std::fs::symlink_metadata(&git_dir) {
        Ok(m) => m,
        Err(e) => return Some(format!(".git cannot be read: {e}")),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Some(".git is a link, not the repository's own directory".to_string());
    }
    if !ft.is_dir() {
        return Some(
            ".git is a file naming another directory, not the repository's own directory"
                .to_string(),
        );
    }
    for name in REDIRECTS {
        if std::fs::symlink_metadata(git_dir.join(name)).is_ok() {
            return Some(format!(
                ".git holds a {name} file that names another directory"
            ));
        }
    }
    None
}

/// [`git_dir_problem`] as an error: the check before any agentd git call on the engine's repository.
pub fn check_git_dir(repo: &Path) -> Result<(), WsError> {
    match git_dir_problem(repo) {
        Some(why) => Err(WsError::Untrusted(why)),
        None => Ok(()),
    }
}

/// `git rev-parse --show-toplevel` of the worktree: the directory the engine's session starts in.
pub async fn toplevel(wt: &Worktree) -> Result<String, WsError> {
    check_git_dir(&wt.repo)?;
    git(&wt.repo, &["rev-parse", "--show-toplevel"]).await
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Copies `src` into the worktree, reusing a worktree the session already has (fast path).
pub async fn prepare_local(
    workspace_root: &Path,
    run: &str,
    agent: &str,
    src: &Path,
) -> Result<Worktree, WsError> {
    let wt = layout(workspace_root, run, agent);
    if wt.repo.join(".git").exists() {
        return Ok(wt);
    }
    copy_tree(src, &wt.repo).map_err(io)?;
    if !wt.repo.join(".git").exists() {
        git(&wt.repo, &["init", "-q", "-b", "main"]).await?;
        git(&wt.repo, &["add", "-A"]).await?;
        git(
            &wt.repo,
            &["commit", "-q", "--allow-empty", "-m", "harness: base"],
        )
        .await?;
    }
    git(&wt.repo, &["checkout", "-q", "-B", &wt.branch]).await?;
    Ok(wt)
}

/// Gives the worktree's repository a local identity, the one the engine's work is committed as ([`AGENT_AUTHOR`]), so that `git commit`
/// in the engine's shell works with no author or committer variable and no global configuration (a direct session has none, so the engine
/// would commit with a one-off identity). Idempotent; it leaves every other setting alone.
/// The harness's own commits keep [`HARNESS_AUTHOR`] through the environment, which outranks this configuration.
pub async fn set_repo_identity(wt: &Worktree) -> Result<(), WsError> {
    check_git_dir(&wt.repo)?;
    git(&wt.repo, &["config", "user.name", AGENT_AUTHOR.0]).await?;
    git(&wt.repo, &["config", "user.email", AGENT_AUTHOR.1]).await?;
    Ok(())
}

/// Clones a git bundle into the worktree, reusing one the session already has.
pub async fn prepare_from_bundle(
    workspace_root: &Path,
    run: &str,
    agent: &str,
    bundle: &Path,
) -> Result<Worktree, WsError> {
    prepare_from_bundle_state(workspace_root, run, agent, bundle)
        .await
        .map(|(wt, _)| wt)
}

/// [`prepare_from_bundle`], and whether this call made the checkout: `false` is a checkout an earlier invocation of the run left, which the engine
/// has worked in since.
pub async fn prepare_from_bundle_state(
    workspace_root: &Path,
    run: &str,
    agent: &str,
    bundle: &Path,
) -> Result<(Worktree, bool), WsError> {
    let wt = layout(workspace_root, run, agent);
    if wt.repo.join(".git").exists() {
        // A checkout of an earlier invocation of the run: the engine has had it since, and it must still be the repository's own directory.
        check_git_dir(&wt.repo)?;
        return Ok((wt, false));
    }
    std::fs::create_dir_all(&wt.root).map_err(io)?;
    let bundle_arg = bundle.display().to_string();
    let repo_arg = wt.repo.display().to_string();
    git(&wt.root, &["clone", "-q", &bundle_arg, &repo_arg]).await?;
    // A bundle without HEAD clones to an unborn HEAD; start from its first ref instead.
    let base = match git(&wt.repo, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]).await {
        Ok(sha) => sha,
        Err(_) => {
            let heads = git(&wt.repo, &["bundle", "list-heads", &bundle_arg]).await?;
            heads
                .split_whitespace()
                .next()
                .ok_or_else(|| WsError::Git("bundle has no refs".into()))?
                .to_string()
        }
    };
    git(&wt.repo, &["checkout", "-q", "-B", &wt.branch, &base]).await?;
    Ok((wt, true))
}

/// Paths moved out of the repository for one turn.
#[derive(Debug, Default)]
pub struct Hidden {
    names: Vec<String>,
    /// `(relative path, original, stash)` of every path that existed and was moved aside.
    moved: Vec<(String, PathBuf, PathBuf)>,
}

impl Hidden {
    /// The hidden paths that existed at launch and were moved aside, sorted.
    pub fn quarantined(&self) -> Vec<String> {
        let mut v: Vec<String> = self.moved.iter().map(|(n, _, _)| n.clone()).collect();
        v.sort();
        v
    }
}

/// A profile's hidden path: a relative path, or a directory written as `<dir>/**`. Any other glob
/// is refused, so a pattern can never silently match nothing.
fn hidden_path(p: &str) -> Result<String, WsError> {
    let path = p.strip_suffix("/**").unwrap_or(p);
    if path.contains(['*', '?', '[', ']']) {
        return Err(WsError::Hidden(p.to_string()));
    }
    check_relative(path).map_err(|_| WsError::Hidden(p.to_string()))?;
    Ok(path.to_string())
}

fn check_relative(p: &str) -> Result<(), WsError> {
    let path = Path::new(p);
    if p.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(WsError::Hidden(p.to_string()));
    }
    Ok(())
}

/// The hidden paths of each worktree's current turn, by repository path, as [`hide_paths`] last
/// moved them aside. It lives in agentd's memory, which the engine user cannot write (the stash and
/// the worktree root are the engine's during the turn), so [`apply_staged_exec`] can leave a
/// restored hidden file exactly as it was.
static HIDDEN_NAMES: LazyLock<Mutex<HashMap<PathBuf, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn hidden_names(repo: &Path) -> Vec<String> {
    HIDDEN_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo)
        .cloned()
        .unwrap_or_default()
}

/// Moves every existing hidden path to `<root>/hidden/` so the engine cannot load it.
pub fn hide_paths(wt: &Worktree, paths: &BTreeSet<String>) -> Result<Hidden, WsError> {
    let paths: BTreeSet<String> = paths
        .iter()
        .map(|p| hidden_path(p))
        .collect::<Result<_, _>>()?;
    HIDDEN_NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(wt.repo.clone(), paths.iter().cloned().collect());
    let stash = wt.root.join("hidden");
    let mut hidden = Hidden::default();
    for p in &paths {
        hidden.names.push(p.clone());
        let src = wt.repo.join(p);
        if std::fs::symlink_metadata(&src).is_ok() {
            let dest = stash.join(p);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::rename(&src, &dest).map_err(io)?;
            hidden.moved.push((p.clone(), src, dest));
        }
    }
    Ok(hidden)
}

fn remove(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Drops whatever the engine wrote at hidden paths and puts the originals back.
pub fn restore_hidden(wt: &Worktree, hidden: Hidden) -> Result<(), WsError> {
    for name in &hidden.names {
        remove(&wt.repo.join(name)).map_err(io)?;
    }
    for (_, original, stashed) in hidden.moved {
        if let Some(parent) = original.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        std::fs::rename(&stashed, &original).map_err(io)?;
    }
    let _ = std::fs::remove_dir_all(wt.root.join("hidden"));
    Ok(())
}

/// Who a turn's work is, for the trailers of agentd's `harness: turn N` commit ([`commit_turn_as`]):
/// the turn's role (`TurnSpec.role`), its agent (`InvocationRequest.agent_id`) and, when the turn names
/// one, its task. The commit's subject and author stay `harness: turn N` and [`AGENT_AUTHOR`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnAuthor<'a> {
    pub role: &'a str,
    pub agent: &'a str,
    pub task: Option<&'a str>,
}

/// A trailer value agentd writes: 1 to 128 characters of `[A-Za-z0-9._:/@+-]`. Anything else (a
/// newline that would start a forged trailer, a control character, a space) is left out, never
/// rewritten.
fn trailer_value(v: &str) -> Option<&str> {
    let ok = !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._:/@+-".contains(&c));
    ok.then_some(v)
}

/// The message of a turn commit: `harness: turn <n>`, and with an author a blank line and the trailers
/// `Ham-Role`, `Ham-Agent`, `Ham-Turn` and `Ham-Task` (each valid value; no task unless the turn names one).
pub fn turn_message(turn: u32, by: Option<&TurnAuthor<'_>>) -> String {
    let mut m = format!("harness: turn {turn}");
    if let Some(b) = by {
        let mut trailers: Vec<String> = Vec::new();
        if let Some(r) = trailer_value(b.role) {
            trailers.push(format!("Ham-Role: {r}"));
        }
        if let Some(a) = trailer_value(b.agent) {
            trailers.push(format!("Ham-Agent: {a}"));
        }
        trailers.push(format!("Ham-Turn: {turn}"));
        if let Some(t) = b.task.and_then(trailer_value) {
            trailers.push(format!("Ham-Task: {t}"));
        }
        m.push_str("\n\n");
        m.push_str(&trailers.join("\n"));
    }
    m
}

/// Commits every change as `harness: turn <n>`; with no change it returns HEAD unchanged.
pub async fn commit_turn(wt: &Worktree, turn: u32) -> Result<String, WsError> {
    commit_turn_with(wt, turn, None, None).await
}

/// [`commit_turn`], with the trailers that name who the turn's work is ([`turn_message`]), so a reader of
/// the history (an evaluator, the role record) can tell which role wrote it even when the role cannot run
/// git itself. A turn whose work the engine already committed, or that changed nothing, makes no commit.
pub async fn commit_turn_as(
    wt: &Worktree,
    turn: u32,
    by: &TurnAuthor<'_>,
) -> Result<String, WsError> {
    commit_turn_with(wt, turn, Some(by), None).await
}

/// [`commit_turn_as`] for the end of a run: the repository's own settings are compared with `state` (the bytes of the start of the run) right
/// before the call that applies clean filters (`git add -A`) and right before the commit, in addition to the check of `.git` that every
/// commit makes. The first difference is an error and nothing is committed.
pub async fn commit_turn_verified(
    wt: &Worktree,
    turn: u32,
    by: &TurnAuthor<'_>,
    state: &GitState,
) -> Result<String, WsError> {
    commit_turn_with(wt, turn, Some(by), Some(state)).await
}

async fn commit_turn_with(
    wt: &Worktree,
    turn: u32,
    by: Option<&TurnAuthor<'_>>,
    state: Option<&GitState>,
) -> Result<String, WsError> {
    // Every step below that runs git starts from a repository that is still its own: the walk of the index can take a while on a large tree.
    let ready = || -> Result<(), WsError> {
        match state {
            Some(s) => s.verify(wt),
            None => check_git_dir(&wt.repo),
        }
    };
    ready()?;
    apply_staged_exec(wt).await?;
    ready()?;
    git(&wt.repo, &["add", "-A"]).await?;
    if !git(&wt.repo, &["status", "--porcelain"]).await?.is_empty() {
        ready()?;
        git(&wt.repo, &["commit", "-q", "-m", &turn_message(turn, by)]).await?;
    }
    git(&wt.repo, &["rev-parse", "HEAD"]).await
}

/// The engine user of each worktree under the uid split, by repository path, as [`set_engine_uid`] last
/// recorded it. Like [`HIDDEN_NAMES`] it lives in agentd's memory, which the engine user cannot write.
static ENGINE_UIDS: LazyLock<Mutex<HashMap<PathBuf, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records that the engine user `uid` owns the worktree for its turns: turn setup gives it the worktree root
/// (the uid split, `engine::chown_tree`). [`apply_staged_exec`] then changes the mode only of a file `uid`
/// owns, a change the engine user could make itself. Without a split nothing is recorded, and every file of
/// the worktree is agentd's own.
pub fn set_engine_uid(wt: &Worktree, uid: u32) {
    ENGINE_UIDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(wt.repo.clone(), uid);
}

fn engine_uid(repo: &Path) -> Option<u32> {
    ENGINE_UIDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo)
        .copied()
}

/// An error a staged entry's walk expects when the engine renamed, removed or replaced a name (missing, a
/// symlink under `O_NOFOLLOW`, not a directory, unreadable): the entry is left alone. Any other error fails.
fn renamed_away(e: Errno) -> bool {
    matches!(
        e,
        Errno::NOENT
            | Errno::LOOP
            | Errno::NOTDIR
            | Errno::ISDIR
            | Errno::ACCESS
            | Errno::PERM
            | Errno::NXIO
            | Errno::NAMETOOLONG
    )
}

fn walk_error(rel: &str, e: Errno) -> WsError {
    WsError::Io(format!("{rel}: {e}"))
}

/// The regular file at `rel` (a repository-relative index path), opened from the handle `repo`: each
/// directory relative to the one before with `O_NOFOLLOW`, then the file with `O_NOFOLLOW`. No symlink is
/// followed, and a name the engine swaps after a step cannot redirect the handle that step returned.
/// `None` when a component is empty, `.`, `..` or `.git`, or the walk meets a name that
/// is missing, a symlink, not a directory, or (the last) not a regular file.
fn open_staged(repo: &OwnedFd, rel: &[u8], shown: &str) -> Result<Option<OwnedFd>, WsError> {
    let parts: Vec<&[u8]> = rel.split(|c| *c == b'/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == b"." || *p == b".." || p.eq_ignore_ascii_case(b".git"))
    {
        return Ok(None);
    }
    let Some((name, dirs)) = parts.split_last() else {
        return Ok(None);
    };
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut cur = repo.try_clone().map_err(io)?;
    for d in dirs {
        cur = match openat(
            &cur,
            std::ffi::OsStr::from_bytes(d),
            dir_flags,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) if renamed_away(e) => return Ok(None),
            Err(e) => return Err(walk_error(shown, e)),
        };
    }
    let name = std::ffi::OsStr::from_bytes(name);
    // Only a regular file is opened (no FIFO or device open runs); the handle's own `fstat` decides.
    match statat(&cur, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile => {}
        Ok(_) => return Ok(None),
        Err(e) if renamed_away(e) => return Ok(None),
        Err(e) => return Err(walk_error(shown, e)),
    }
    let file_flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    match openat(&cur, name, file_flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if renamed_away(e) => Ok(None),
        Err(e) => Err(walk_error(shown, e)),
    }
}

/// Gives the exec bit on disk to every regular file the index stages as `100755` (stage 0) whose file
/// lacks it, and returns those paths, sorted. The engine's editor writes files 0644, so an agent marks a file executable in the index (`git update-index
/// --chmod=+x`, or a commit of its own); `git add -A` would put the disk mode back (`mode change 100755 => 100644 mvnw` on
/// every turn). A path is therefore
/// committed executable when its file or its index entry is. It only adds: a 0755 file the index marks
/// `100644` stays as it is.
///
/// agentd runs as root and engine-user processes can outlive the engine, so every check is made on the
/// handle that is changed ([`open_staged`], then `fstat` and `fchmod` on that handle): a regular file with
/// one link (a second name shares the inode), without the user exec bit and, under the uid split, owned by
/// the engine user ([`set_engine_uid`]). Symlinks, gitlinks, missing files, paths through a symlinked
/// directory or `.git`, and the hidden paths [`hide_paths`] moved aside this turn are left alone; set-ID
/// bits are cleared. A name the engine renames or replaces during the walk leaves its entry alone (the
/// helper does not touch the disk for it).
pub async fn apply_staged_exec(wt: &Worktree) -> Result<Vec<String>, WsError> {
    check_git_dir(&wt.repo)?;
    let raw = git_bytes(&wt.repo, &["ls-files", "-s", "-z"]).await?;
    let hidden = hidden_names(&wt.repo);
    let owner = engine_uid(&wt.repo);
    let repo = openat(
        CWD,
        wt.repo.as_path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| WsError::Io(format!("{}: {e}", wt.repo.display())))?;
    let mut changed = Vec::new();
    for rec in raw.split(|c| *c == 0).filter(|r| !r.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let Some(tab) = rec.iter().position(|c| *c == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&rec[..tab]);
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields.len() != 3 || fields[0] != "100755" || fields[2] != "0" {
            continue;
        }
        let rel = &rec[tab + 1..];
        let shown = String::from_utf8_lossy(rel).into_owned();
        if hidden
            .iter()
            .any(|h| shown == *h || shown.starts_with(&format!("{h}/")))
        {
            continue;
        }
        let Some(fd) = open_staged(&repo, rel, &shown)? else {
            continue;
        };
        let st = fstat(&fd).map_err(|e| walk_error(&shown, e))?;
        let mode = st.st_mode;
        if FileType::from_raw_mode(mode) != FileType::RegularFile
            || st.st_nlink != 1
            || mode & 0o100 != 0
            || owner.is_some_and(|uid| st.st_uid != uid)
        {
            continue;
        }
        fchmod(&fd, Mode::from_raw_mode((mode & 0o777) | 0o111))
            .map_err(|e| walk_error(&shown, e))?;
        changed.push(shown);
    }
    changed.sort();
    Ok(changed)
}

/// The paths that differ between `base` and HEAD (renames split, added and deleted alike).
pub async fn changed_between(wt: &Worktree, base: &str) -> Result<BTreeSet<String>, WsError> {
    check_git_dir(&wt.repo)?;
    let raw = git_bytes(
        &wt.repo,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            base,
            "HEAD",
        ],
    )
    .await?;
    Ok(nul_list(&raw).collect())
}

/// Writes a bundle of the harness branch.
pub async fn write_bundle(wt: &Worktree, out: &Path) -> Result<(), WsError> {
    check_git_dir(&wt.repo)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let out_arg = out.display().to_string();
    git(&wt.repo, &["bundle", "create", "-q", &out_arg, &wt.branch])
        .await
        .map(|_| ())
}

/// HEAD of the worktree: the base a turn's changes are measured from.
pub async fn head(wt: &Worktree) -> Result<String, WsError> {
    check_git_dir(&wt.repo)?;
    git(&wt.repo, &["rev-parse", "HEAD"]).await
}

/// Whether `sha` is a full commit id: 40 (SHA-1) or 64 (SHA-256) lower-case hexadecimal digits.
fn full_commit_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The message of commit `sha` (`git show -s --format=%B`), which must be a full commit id: a name, a revision expression or an option is
/// refused before git is run. The message is the engine's text; it is read for the flow it declares and nothing else.
pub async fn commit_message(wt: &Worktree, sha: &str) -> Result<String, WsError> {
    if !full_commit_id(sha) {
        return Err(WsError::Git("show: not a full commit id".to_string()));
    }
    check_git_dir(&wt.repo)?;
    git(&wt.repo, &["show", "-s", "--no-patch", "--format=%B", sha]).await
}

/// The repository's local config and info/attributes as the turn found them. The engine can
/// rewrite both (a clean filter in `.git/config` runs on `git add`); `restore` puts them back
/// before agentd's own git calls and checks that it did, and `verify` compares them with these bytes again.
///
/// Neither call follows a link: the engine can replace a file or `.git/info` by a link to a file of agentd's, and agentd writes as the
/// user that owns it. `.git` itself must be the repository's own plain directory ([`git_dir_problem`]); a redirect there is an error,
/// never something to repair, because the configuration git would read is not in the files this holds.
#[derive(Debug)]
pub struct GitState {
    config: Option<Vec<u8>>,
    attributes: Option<Vec<u8>>,
}

/// The bytes of a regular file read without following a link at the last name; `None` when it is not there. A link, a directory or a device
/// in its place is an error: it is not what the start of the run found.
fn read_opt(p: &Path) -> Result<Option<Vec<u8>>, WsError> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => {
            return Err(WsError::Untrusted(format!(
                "{} is not a plain file",
                p.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
        .map_err(io)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut bytes).map_err(io)?;
    Ok(Some(bytes))
}

/// Takes the entry at `p` away without following it: a link goes, not what it names; a directory goes with what it holds (links inside it are
/// removed, not followed). Nothing there is nothing to do.
fn remove_entry(p: &Path) -> Result<(), WsError> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_dir() => std::fs::remove_dir_all(p).map_err(io),
        Ok(_) => std::fs::remove_file(p).map_err(io),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(e)),
    }
}

/// A new plain file with `bytes` at `p`: made with `O_EXCL` and `O_NOFOLLOW`, so a name that appeared since the entry was removed is an error and
/// not a write into whatever the name leads to.
fn write_new(p: &Path, bytes: &[u8]) -> Result<(), WsError> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
        .map_err(io)?;
    f.write_all(bytes).map_err(io)
}

/// `<repo>/.git/info` when it is the plain directory it must be, `None` when it is not there; anything else (a link, a file) is an error.
fn info_dir(git_dir: &Path) -> Result<Option<PathBuf>, WsError> {
    let info = git_dir.join("info");
    match std::fs::symlink_metadata(&info) {
        Ok(m) if m.file_type().is_dir() => Ok(Some(info)),
        Ok(_) => Err(WsError::Untrusted(
            ".git/info is not the repository's own directory".to_string(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e)),
    }
}

impl GitState {
    pub fn snapshot(wt: &Worktree) -> Result<Self, WsError> {
        check_git_dir(&wt.repo)?;
        let git_dir = wt.repo.join(".git");
        let attributes = match info_dir(&git_dir)? {
            Some(info) => read_opt(&info.join("attributes"))?,
            None => None,
        };
        Ok(Self {
            config: read_opt(&git_dir.join("config"))?,
            attributes,
        })
    }

    /// Puts both files back, without following a link, and checks that they read as they did at the start. An error leaves the checkout as the
    /// engine left it: the caller does not commit in it.
    pub fn restore(&self, wt: &Worktree) -> Result<(), WsError> {
        check_git_dir(&wt.repo)?;
        let git_dir = wt.repo.join(".git");
        let config = git_dir.join("config");
        remove_entry(&config)?;
        if let Some(bytes) = &self.config {
            write_new(&config, bytes)?;
        }
        let info = info_dir(&git_dir)?;
        match (&self.attributes, info) {
            (Some(bytes), Some(info)) => {
                let attributes = info.join("attributes");
                remove_entry(&attributes)?;
                write_new(&attributes, bytes)?;
            }
            (Some(bytes), None) => {
                let info = git_dir.join("info");
                std::fs::create_dir(&info).map_err(io)?;
                write_new(&info.join("attributes"), bytes)?;
            }
            (None, Some(info)) => remove_entry(&info.join("attributes"))?,
            (None, None) => {}
        }
        self.verify(wt)
    }

    /// Whether `.git` is still the repository's own directory and both files hold the bytes of the start of the run. The first difference is the
    /// error.
    pub fn verify(&self, wt: &Worktree) -> Result<(), WsError> {
        check_git_dir(&wt.repo)?;
        let git_dir = wt.repo.join(".git");
        let differs = |name: &str| {
            WsError::Untrusted(format!(".git/{name} is not what the run started with"))
        };
        if read_opt(&git_dir.join("config"))? != self.config {
            return Err(differs("config"));
        }
        let attributes = match info_dir(&git_dir)? {
            Some(info) => read_opt(&info.join("attributes"))?,
            None => None,
        };
        if attributes != self.attributes {
            return Err(differs("info/attributes"));
        }
        Ok(())
    }
}

fn nul_list(b: &[u8]) -> impl Iterator<Item = String> + '_ {
    b.split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}
