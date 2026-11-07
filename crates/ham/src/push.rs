//! `ham push <runId> <git-url> [--branch <name>]`: the run's
//! code in another git repository, in one step, with no git host built into the harness
//! and no token in `ham`.
//!
//! The flow reuses [`crate::clone`]:
//!
//! 1. The URL and the branch name are checked (`ham_core::push`): a URL that carries a credential is refused before anything is
//!    downloaded, and no message repeats it. A path on this machine is made absolute against the working directory.
//! 2. [`crate::clone::clone_run`] downloads the zip like `ham pull` (it stays in the download directory) and builds the repository from its
//!    `repo.bundle`, proven equal to the zip's `src/`, in a temporary directory `.ham-push-<runId>-<pid>-<nanos>/repo` beside the zip.
//!    The directory is removed on every exit path (`Drop`); only a killed process leaves one, which is safe to delete.
//! 3. `git push -- <url> refs/heads/harness/<runId>:refs/heads/<branch>` runs in that repository with the USER's git: their global
//!    configuration, credential helper, `GIT_SSH_COMMAND` and ssh agent (the clone step shuts the machine's configuration out; the push
//!    cannot, because the configuration is how the user logs in). Only the variables that choose WHICH repository git works on
//!    (`GIT_DIR`, `GIT_WORK_TREE`, ...) are removed. No remote is added, so the URL is not stored anywhere. The push is never forced:
//!    a remote branch with other history rejects it, and git's own message says so. git's output goes to the user's terminal on stderr,
//!    so `--json` keeps standard output to the one result line.

use crate::api::Api;
use crate::clone::{CloneError, Cloned, Git, RunRequest, clone_run};
use crate::http::ControlPlane;
use crate::pull::Pulled;
use ham_core::push::{check_branch, check_git_url, default_branch, is_local_path};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// What `ham push` can fail on. The text is for the person at the terminal; none of it repeats the URL.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The URL or the branch name is not acceptable.
    #[error("{0}")]
    Usage(String),
    /// The zip, the repository or git (before the push) failed.
    #[error(transparent)]
    Clone(#[from] CloneError),
    /// `git push` could not run, or ended with an error. git has already told the person why, on stderr.
    #[error("{0}")]
    Push(String),
}

/// The request of [`push_run`].
#[derive(Clone, Copy, Debug)]
pub struct PushRequest<'a> {
    pub run_id: &'a str,
    /// The URL as the person typed it.
    pub url: &'a str,
    /// The remote branch; `None` means the run's own branch.
    pub branch: Option<&'a str>,
    /// Where the zip is downloaded (the pull rules); the temporary repository is made beside it.
    pub downloads: &'a Path,
    pub cwd: &'a Path,
    pub chunk: u64,
}

/// A finished push.
#[derive(Clone, Debug)]
pub struct Pushed {
    pub url: String,
    /// The remote branch.
    pub branch: String,
    pub cloned: Cloned,
    pub pulled: Pulled,
    /// Why the temporary directory could not be removed, if it could not.
    pub leftover: Option<String>,
}

/// The temporary directory of one push; removed when dropped.
struct TempRoot {
    path: PathBuf,
    armed: bool,
}

impl TempRoot {
    fn new(downloads: &Path, run_id: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Self {
            path: downloads.join(format!(".ham-push-{run_id}-{}-{nanos}", std::process::id())),
            armed: true,
        }
    }

    /// Removes the directory now; the message when that fails.
    fn discard(&mut self) -> Option<String> {
        self.armed = false;
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!(
                "could not remove the temporary directory {}: {e}",
                self.path.display()
            )),
        }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The variables that choose which repository git works on. The push runs in a repository `ham` made, so the machine's choice is set
/// aside; everything else (credential helpers, ssh, proxies, `GIT_CONFIG_*`) is the user's and stays.
const REPOSITORY_VARIABLES: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
];

/// The URL as git is given it: a path on this machine is made absolute against `cwd` (git runs inside the temporary repository).
fn url_for_git(url: &str, cwd: &Path) -> String {
    if is_local_path(url) {
        let joined = cwd.join(url);
        std::path::absolute(&joined)
            .unwrap_or(joined)
            .display()
            .to_string()
    } else {
        url.to_string()
    }
}

/// `git push` of `branch` to `url` from `repo`, with the user's own git. Blocking.
fn git_push(
    git: &Git,
    repo: &Path,
    url: &str,
    local_branch: &str,
    remote_branch: &str,
) -> Result<(), PushError> {
    let mut c = std::process::Command::new(git.program());
    for v in REPOSITORY_VARIABLES {
        c.env_remove(v);
    }
    let refspec = format!("refs/heads/{local_branch}:refs/heads/{remote_branch}");
    c.current_dir(repo).args(["push", "--", url, &refspec]);
    // git writes its result lines on stderr; anything it writes on stdout goes there too, so standard output stays `ham`'s.
    let out = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_or_else(|_| Stdio::null(), Stdio::from);
    c.stdout(out);
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(PushError::Push(format!(
            "git push failed ({}). ham never forces a push: if the branch already has other history, push to a new branch with --branch <name>. Otherwise check that the URL is right and that your own git credentials (your SSH key or credential helper) may write to it",
            s.code()
                .map_or_else(|| "ended by a signal".to_string(), |c| format!("exit {c}"))
        ))),
        Err(e) => Err(PushError::Push(format!(
            "could not run {}: {e}",
            git.program().to_string_lossy()
        ))),
    }
}

/// Pushes the run's branch to `req.url` (see the module doc). `notice` receives what the person should see before the work starts
/// (a partial delivery).
pub async fn push_run<P: ControlPlane>(
    api: &Api<P>,
    git: &Git,
    req: &PushRequest<'_>,
    notice: &mut (dyn FnMut(&str) + Send),
) -> Result<Pushed, PushError> {
    check_git_url(req.url).map_err(|p| PushError::Usage(format!("ham push: {p}")))?;
    let remote_branch = req
        .branch
        .map_or_else(|| default_branch(req.run_id), str::to_string);
    check_branch(&remote_branch).map_err(|e| PushError::Usage(format!("--branch {e}")))?;
    let mut temp = TempRoot::new(req.downloads, req.run_id);
    let dest = temp.path.join("repo");
    let mut say = |n: &str| notice(&n.replace("cloning", "pushing"));
    let (cloned, pulled) = clone_run(
        api,
        git,
        &RunRequest {
            run_id: req.run_id,
            dest: Some(&dest),
            downloads: req.downloads,
            cwd: req.cwd,
            chunk: req.chunk,
        },
        &mut say,
    )
    .await?;
    let (git2, repo, url, local) = (
        git.clone(),
        cloned.dir.clone(),
        url_for_git(req.url, req.cwd),
        cloned.branch.clone(),
    );
    let remote = remote_branch.clone();
    tokio::task::spawn_blocking(move || git_push(&git2, &repo, &url, &local, &remote))
        .await
        .map_err(|e| PushError::Push(format!("the push task failed: {e}")))??;
    Ok(Pushed {
        url: req.url.to_string(),
        branch: remote_branch,
        cloned,
        pulled,
        leftover: temp.discard(),
    })
}
