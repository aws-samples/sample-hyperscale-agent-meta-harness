//! The progress fingerprint of the goal loop: a hash of the work tree that ignores what is not the work. The supervisor
//! compares it between rounds: a round that changed no file is a round that made no progress. The goal card, the run log and the other
//! records of the lifecycle pack live in `.ham/lifecycle/` and change every round, so they are left out;
//! so are the paths agentd moved aside at launch (the end sequence drops what the engine writes there).
//!
//! It is made in a private repository of agentd's (`<agent data>/goal.git`, the engine's objects as an alternate, its own index), never in
//! the engine's repository, as the nudge's change test and the snapshot do.

use crate::direct::records::{EARLIER_BASE, Records};
use crate::direct::snapshot::{ScratchRepo, read_regular_nofollow};
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
        ScratchRepo::open(git_dir.clone(), repo).await?;
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
        })
    }

    /// The tree id of the work tree as it is now, less the excluded paths; `None` when it could not be made.
    pub async fn fingerprint(&self) -> Option<String> {
        let _one_at_a_time = self.lock.lock().await;
        tokio::time::timeout(FINGERPRINT_TIMEOUT, self.make())
            .await
            .ok()?
            .ok()
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
