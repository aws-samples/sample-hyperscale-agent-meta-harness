//! The progress fingerprint of the goal loop: a hash of the work tree that ignores what is not the work. The supervisor
//! compares it between rounds: a round that changed no file is a round that made no progress. The goal card, the run log and the other
//! records of the lifecycle pack live in `.ham/lifecycle/` and change every round, so they are left out;
//! so are the paths agentd moved aside at launch (the end sequence drops what the engine writes there).
//!
//! It is made in a private repository of agentd's (`<agent data>/goal.git`, the engine's objects as an alternate, its own index), never in
//! the engine's repository, as the nudge's change test and the snapshot do.

use crate::direct::records::{EARLIER_BASE, Records};
use crate::direct::snapshot::{ScratchRepo, TidyMarks, read_regular_nofollow};
use crate::workspace::git_with_env;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The folder of the lifecycle pack's records when no `.ham/records` names another.
pub const LIFECYCLE_DIR: &str = ".ham/lifecycle";
/// The folder the records had. A rules file the user edited keeps the old paths, and the log it writes there changes every round.
pub const LEGACY_LIFECYCLE_DIR: &str = "docs/lifecycle";
/// A fingerprint that takes longer than this is no fingerprint.
pub const FINGERPRINT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct TreeFp {
    git_dir: PathBuf,
    repo: PathBuf,
    excludes: Vec<String>,
    lock: tokio::sync::Mutex<()>,
    /// The private repository itself, for its clean-up ([`ScratchRepo::tidy_logged`]).
    scratch: ScratchRepo,
}

/// The pathspec of a hidden path (`dir/**` is the directory), or `None` for one with pattern characters.
fn exclude_spec(path: &str) -> Option<String> {
    let p = path.strip_suffix("/**").unwrap_or(path).trim_matches('/');
    let plain = !p.is_empty()
        && !p.split('/').any(|c| c == ".." || c == ".")
        && !p.contains(['*', '?', '[', ']', '\\', ':']);
    plain.then(|| format!(":(exclude){p}"))
}

impl TreeFp {
    /// Makes the private repository for the work tree at `repo`; `hidden` are the paths agentd moves aside.
    pub async fn open(agent_data: &Path, repo: &Path, hidden: &[String]) -> Result<Self, String> {
        let git_dir = agent_data.join("goal.git");
        // The settings of a private repository of a working session (`ScratchRepo::tune`).
        let scratch = ScratchRepo::open(git_dir.clone(), repo)
            .await?
            .tuned()
            .await;
        // The records of the earlier folder do not count either (COMPAT-CAH-DATA): a repository that keeps them there is worked in that folder.
        let earlier = format!("{EARLIER_BASE}/lifecycle");
        let excludes = [LIFECYCLE_DIR, earlier.as_str(), LEGACY_LIFECYCLE_DIR]
            .into_iter()
            .chain(hidden.iter().map(String::as_str))
            .filter_map(exclude_spec)
            .collect();
        Ok(Self {
            git_dir,
            repo: repo.to_path_buf(),
            excludes,
            lock: tokio::sync::Mutex::new(()),
            scratch,
        })
    }

    /// Other marks for the clean-up of the private repository than those of a run (`snapshot::TidyMarks::RUN`); tests.
    #[must_use]
    pub fn with_tidy_marks(mut self, marks: TidyMarks) -> Self {
        self.scratch.set_tidy_marks(marks);
        self
    }

    /// Another limit for the clean-up of the private repository than `snapshot::TIDY_LIMIT`; tests.
    #[must_use]
    pub fn with_tidy_limit(mut self, limit: Duration) -> Self {
        self.scratch.set_tidy_limit(limit);
        self
    }

    /// The tree id of the work tree as it is now, less the excluded paths; `None` when it could not be made.
    pub async fn fingerprint(&self) -> Option<String> {
        let _one_at_a_time = self.lock.lock().await;
        let made = tokio::time::timeout(FINGERPRINT_TIMEOUT, self.make())
            .await
            .ok()?
            .ok()?;
        // Each fingerprint of a changed tree leaves the new versions of the changed files in this repository, and nothing removed
        // them: a run that rewrites a large file between its rounds filled the worker's disk that way. The clean-up runs when the
        // repository has grown by a mark, under the lock that serializes the fingerprints, so no prune runs beside a `git add`. It
        // runs here, once the fingerprint is made and outside the time a fingerprint has: inside it, a clean-up that ran long turned
        // a fingerprint that was made into none and was ended in the middle. It has a limit of its own (`snapshot::TIDY_LIMIT`),
        // changes no fingerprint, and one that fails or is given up is logged once and fails nothing.
        self.scratch.tidy_logged().await;
        Some(made)
    }

    async fn make(&self) -> Result<String, String> {
        let index = self.git_dir.join("index");
        std::fs::create_dir_all(self.git_dir.join("info"))
            .map_err(|e| format!("goal tree: {e}"))?;
        // The repository's ignore rules as they are now, read as a regular file or not at all.
        let exclude =
            read_regular_nofollow(&self.repo.join(".git/info/exclude")).unwrap_or_default();
        std::fs::write(self.git_dir.join("info/exclude"), exclude)
            .map_err(|e| format!("goal tree: {e}"))?;
        let envs: [(&str, &Path); 3] = [
            ("GIT_DIR", &self.git_dir),
            ("GIT_WORK_TREE", &self.repo),
            ("GIT_INDEX_FILE", &index),
        ];
        // The folder the repository names for the records is read each time: the file may be written during the run.
        let moved: Vec<String> = Records::read(&self.repo)
            .progress_excludes()
            .iter()
            .filter_map(|p| exclude_spec(p))
            .collect();
        let mut add = vec!["add", "-A", "--", "."];
        add.extend(self.excludes.iter().map(String::as_str));
        add.extend(moved.iter().map(String::as_str));
        git_with_env(&self.repo, &add, &envs)
            .await
            .map_err(|e| format!("goal tree: {e}"))?;
        git_with_env(&self.repo, &["write-tree"], &envs)
            .await
            .map_err(|e| format!("goal tree: {e}"))
    }
}
