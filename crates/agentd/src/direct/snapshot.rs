//! The periodic snapshot. Every `snapshot_every_s` seconds while the prompt runs, and only when
//! the working tree changed, agentd saves the tree so that a microVM that dies still delivers the work so far.
//!
//! The snapshot never touches the engine's repository: no index, no HEAD, no ref and no object file is written there (a
//! root-owned object directory would stop the engine's own `git commit`). It lives in a bare repository of its own,
//! `<agent_data>/snapshot.git` (root, mode 0700), whose `objects/info/alternates` names the repository's object store
//! ([`ScratchRepo`]). Each snapshot is `git add -A` into that repository's own index (seeded with the base tree, so a file
//! tracked at the base that an ignore rule now matches is not read as a deletion), `write-tree`, `commit-tree` on top of the
//! repository's HEAD, `update-ref refs/heads/snapshot` and a bundle of that ref. The tree is compared with the last
//! snapshot's (the base's for the first), so an idle run makes none.
//!
//! agentd reads engine-controlled files here as root: the repository's `info/exclude` is opened without following a link and
//! without blocking, and only when it is a regular file of at most 1 MiB.

use crate::BoxFuture;
use crate::direct::classify::scrub_detail;
use crate::direct::input::{ChangedFiles, DigestSources, TreeSource};
use crate::workspace::{WsError, git_with_env};
use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most bytes read from an engine-controlled file.
const MAX_CONTROLLED_FILE: u64 = 1 << 20;

/// One snapshot, ready to upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMade {
    pub commit: String,
    pub base: String,
    pub bundle: PathBuf,
    /// 1 for the first snapshot of the run.
    pub count: u32,
}

fn io(e: std::io::Error) -> String {
    format!("snapshot: {e}")
}

fn ws(e: WsError) -> String {
    format!("snapshot: {e}")
}

/// The bytes of a regular file of at most [`MAX_CONTROLLED_FILE`] bytes, opened without following a link and without blocking;
/// `None` for a missing file, a link, a FIFO, a device, a directory or a file that is too big.
pub fn read_regular_nofollow(path: &Path) -> Option<Vec<u8>> {
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let st = fstat(&fd).ok()?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || u64::try_from(st.st_size).ok()? > MAX_CONTROLLED_FILE
    {
        return None;
    }
    let mut out = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_CONTROLLED_FILE)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// The most bytes of `git log` lines a stop's facts carry.
pub const MAX_LOG_BYTES: usize = 64 * 1024;
/// The most bytes of `git diff --stat` a stop's facts carry.
pub const MAX_STAT_BYTES: usize = 64 * 1024;
/// The most file lines of `git diff --stat` a stop's facts carry.
pub const MAX_STAT_FILES: usize = 500;
/// The most paths or log lines one call returns, whatever `limit` asks.
pub const MAX_FACT_ITEMS: usize = 2_000;
/// The file at the top of the work tree that an agent keeps its progress in, by convention.
pub const PROGRESS_FILE: &str = "PROGRESS.md";
/// The most bytes of the progress file a stop's facts carry.
pub const MAX_PROGRESS_BYTES: usize = 32 * 1024;

/// `Ok` for an object id (7 to 64 hexadecimal digits), so a value that reaches git's command line can never be an option or a ref.
fn object_id(s: &str) -> Result<(), String> {
    if (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("snapshot: not an object id: {}", s.escape_debug()))
    }
}

/// The head of `PROGRESS.md` at the top of the work tree at `repo` (at most [`MAX_PROGRESS_BYTES`] bytes, cut at a character
/// boundary), when it is a regular file: engine-controlled, so opened as `read_regular_nofollow` does. `None` when absent or not
/// a regular file. Not scrubbed here; the digest builder scrubs the whole text.
pub fn progress_file(repo: &Path) -> Option<String> {
    let bytes = read_regular_nofollow(&repo.join(PROGRESS_FILE))?;
    let text = String::from_utf8_lossy(&bytes).replace('\0', "");
    let kept = head_bytes(&text, MAX_PROGRESS_BYTES);
    (!kept.trim().is_empty()).then(|| kept.to_string())
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn head_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `lines` joined with newlines, whole lines only, at most `budget` bytes in all; `last` (a summary line) is always kept and counted.
fn bounded_lines(
    lines: impl Iterator<Item = String>,
    budget: usize,
    last: Option<String>,
) -> String {
    let reserve = last.as_ref().map_or(0, |l| l.len() + 1);
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 + reserve > budget {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    match last {
        Some(l) => out.push_str(&l),
        None => {
            out.pop();
        }
    }
    out
}

/// The most changed paths a stop's facts list (the digest lists them, the card shows the first twelve).
pub const MAX_FACT_PATHS: usize = 500;
/// The most `git log` lines a stop's facts carry.
pub const MAX_FACT_LOG_LINES: usize = 200;

/// What the work tree holds against the base commit when a stop is assessed: the git inputs of the digest of a fresh session and of
/// the card, gathered in one pass by [`ScratchRepo::git_facts`]. A part that could not be read is empty here and named in `errors`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitFacts {
    /// `<short id> <subject>` lines of the commits made since the base, newest first.
    pub git_log: Option<String>,
    /// `git diff --stat` of the work tree against the base.
    pub diff_stat: Option<String>,
    /// The first [`MAX_FACT_PATHS`] paths that differ from the base.
    pub changed_files: Vec<String>,
    /// How many paths differ from the base in all.
    pub files_total: u32,
    /// The head of `PROGRESS.md` at the top of the work tree.
    pub progress: Option<String>,
    /// One scrubbed line per part that failed.
    pub errors: Vec<String>,
}

impl GitFacts {
    /// Puts the git members of a digest's sources (`git_log`, `diff_stat`, `changed_files`, `files_total`, `progress_file`) into `into`;
    /// every other member is left as it is.
    pub fn fill(self, into: &mut DigestSources) {
        into.git_log = self.git_log;
        into.diff_stat = self.diff_stat;
        into.changed_files = self.changed_files;
        into.files_total = self.files_total;
        into.progress_file = self.progress;
    }
}

/// The real tree measurer and git half of the digest sources of a run: a [`ScratchRepo::facts`] repository, the base commit, and a lock
/// so that two callers never share the repository's index at once.
#[derive(Debug)]
pub struct GitSource {
    scratch: ScratchRepo,
    base: String,
    lock: tokio::sync::Mutex<()>,
}

impl GitSource {
    pub fn new(scratch: ScratchRepo, base: impl Into<String>) -> Self {
        Self {
            scratch,
            base: base.into(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Opens `<agent_data>/facts.git` for the work tree at `repo`, seeded with `base`.
    pub async fn open(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        Ok(Self::new(
            ScratchRepo::facts(agent_data, repo, base).await?,
            base,
        ))
    }

    /// Every git fact of the work tree now. The engine's `HEAD` is read with the one `git rev-parse HEAD` agentd already runs in the
    /// engine's repository for the snapshot and the nudge (hooks, fsmonitor and the global configuration off); nothing else is.
    pub async fn facts(&self) -> GitFacts {
        let _guard = self.lock.lock().await;
        match git_with_env(&self.scratch.repo, &["rev-parse", "HEAD"], &[]).await {
            Ok(head) => self.scratch.git_facts(&self.base, &head).await,
            Err(e) => {
                // Without the head there is no log; the rest of the facts do not need it.
                let mut facts = self.scratch.git_facts(&self.base, &self.base).await;
                facts.errors.push(scrub_detail(&format!("head: {}", ws(e))));
                facts
            }
        }
    }
}

impl TreeSource for GitSource {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<ChangedFiles>> {
        Box::pin(async move {
            let _guard = self.lock.lock().await;
            let (paths, total) = self.scratch.changed_paths(&self.base, limit).await.ok()?;
            Some(ChangedFiles { paths, total })
        })
    }
}

/// A private bare repository that can hash the working tree of the engine's repository without writing to it: its own index
/// and object store, the engine's objects as an alternate, the engine's work tree as `GIT_WORK_TREE`.
#[derive(Debug)]
pub struct ScratchRepo {
    git_dir: PathBuf,
    repo: PathBuf,
}

impl ScratchRepo {
    /// `<parent>/probe.git`, for the nudge's change test.
    pub async fn new(parent: &Path, repo: &Path) -> Result<Self, String> {
        Self::open(parent.join("probe.git"), repo).await
    }

    /// Creates (or reuses) the repository at `git_dir` for the work tree at `repo`.
    pub async fn open(git_dir: PathBuf, repo: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&git_dir).map_err(io)?;
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        let gd: &Path = &git_dir;
        let parent = git_dir.parent().unwrap_or(Path::new("/"));
        git_with_env(
            parent,
            &["init", "-q", "--bare", gd.to_str().unwrap_or_default()],
            &[],
        )
        .await
        .map_err(ws)?;
        // A work tree is set per call; the repository is not bare for the calls that add files.
        git_with_env(
            parent,
            &["config", "core.bare", "false"],
            &[("GIT_DIR", gd)],
        )
        .await
        .map_err(ws)?;
        let objects = repo.join(".git/objects");
        std::fs::create_dir_all(git_dir.join("objects/info")).map_err(io)?;
        std::fs::write(
            git_dir.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(io)?;
        Ok(Self {
            git_dir,
            repo: repo.to_path_buf(),
        })
    }

    fn envs<'a>(&'a self, index: &'a Path) -> [(&'static str, &'a Path); 3] {
        [
            ("GIT_DIR", &self.git_dir),
            ("GIT_WORK_TREE", &self.repo),
            ("GIT_INDEX_FILE", index),
        ]
    }

    /// Fills the private index with the tree of `commit`, so what is tracked there stays tracked whatever an ignore rule says.
    pub async fn seed(&self, commit: &str) -> Result<(), String> {
        let index = self.git_dir.join("index");
        git_with_env(&self.repo, &["read-tree", commit], &self.envs(&index))
            .await
            .map(|_| ())
            .map_err(ws)
    }

    /// `<agent_data>/facts.git` for the work tree at `repo`, seeded with the tree of `base`: the repository the git facts of a
    /// stop are read from ([`Self::changed_paths`], [`Self::log_since`], [`Self::diff_stat`]). It is its own repository (its own
    /// index and object store) so that reading the facts never moves the nudge's `probe.git` fingerprint, and the seed keeps a file
    /// that is tracked at `base` tracked whatever an ignore rule says now (without it such a file would read as deleted).
    pub async fn facts(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        let scratch = Self::open(agent_data.join("facts.git"), repo).await?;
        scratch.seed(base).await?;
        Ok(scratch)
    }

    /// A read-only git call on this repository alone: `GIT_DIR` set, run from agentd's own directory (never from the engine's).
    async fn read(&self, args: &[&str]) -> Result<String, String> {
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        git_with_env(cwd, args, &[("GIT_DIR", &self.git_dir)])
            .await
            .map_err(ws)
    }

    /// The tree id of the commit `commit` (an object id, 7 to 64 hexadecimal digits, never a ref name).
    pub async fn tree_of(&self, commit: &str) -> Result<String, String> {
        object_id(commit)?;
        self.read(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
            .await
    }

    /// The paths that differ between the commit `base` and the work tree as it is now (added, modified, deleted, type changed; no
    /// rename detection), in git's path order, at most `limit` of them (each scrubbed to one line), and how many differ in all.
    /// Read through this repository's own index: one caller at a time, and use a repository made by [`Self::facts`] with the same
    /// `base`, otherwise a tracked file an ignore rule matches would be listed as deleted. Nothing is run in the engine's repository.
    pub async fn changed_paths(
        &self,
        base: &str,
        limit: usize,
    ) -> Result<(Vec<String>, u32), String> {
        let base_tree = self.tree_of(base).await?;
        let work_tree = self.tree().await?;
        self.paths_between(&base_tree, &work_tree, limit).await
    }

    /// The paths that differ between two tree ids (see [`Self::changed_paths`]).
    async fn paths_between(
        &self,
        base_tree: &str,
        work_tree: &str,
        limit: usize,
    ) -> Result<(Vec<String>, u32), String> {
        if base_tree == work_tree {
            return Ok((Vec::new(), 0));
        }
        // `--name-status -z` prints `<status> NUL <path> NUL`; the status letter leads the output, so the trimming of the
        // output never eats the start of a path that begins with a space.
        let raw = self
            .read(&[
                "diff-tree",
                "-r",
                "-z",
                "--name-status",
                "--no-renames",
                base_tree,
                work_tree,
            ])
            .await?;
        let mut fields = raw.split('\0');
        let mut paths = Vec::new();
        let mut total: u32 = 0;
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            if status.is_empty() {
                break;
            }
            total = total.saturating_add(1);
            if paths.len() < limit.min(MAX_FACT_ITEMS) {
                paths.push(scrub_detail(path));
            }
        }
        Ok((paths, total))
    }

    /// The commits from `base` (excluded) to `head` (included), one `<short id> <subject>` line each, newest first, at most `limit`
    /// lines and [`MAX_LOG_BYTES`] bytes, every line scrubbed. `head` is an object id the caller read (the engine's `HEAD`); this
    /// repository has no refs of the engine's.
    pub async fn log_since(&self, base: &str, head: &str, limit: usize) -> Result<String, String> {
        object_id(base)?;
        object_id(head)?;
        let count = limit.min(MAX_FACT_ITEMS);
        if count == 0 {
            return Ok(String::new());
        }
        let raw = self
            .read(&[
                "log",
                "--no-color",
                "--no-decorate",
                "--no-show-signature",
                "--format=%h %s",
                &format!("--max-count={count}"),
                &format!("{base}..{head}"),
            ])
            .await?;
        Ok(bounded_lines(
            raw.lines().map(scrub_detail),
            MAX_LOG_BYTES,
            None,
        ))
    }

    /// `git diff --stat` between two tree ids, at most [`MAX_STAT_FILES`] file lines then ` ...` and the summary line, bounded to
    /// [`MAX_STAT_BYTES`] bytes with every line scrubbed; empty when the trees are equal.
    pub async fn diff_stat(&self, base_tree: &str, work_tree: &str) -> Result<String, String> {
        object_id(base_tree)?;
        object_id(work_tree)?;
        if base_tree == work_tree {
            return Ok(String::new());
        }
        let raw = self
            .read(&[
                "diff-tree",
                "-r",
                "--no-renames",
                "--no-color",
                &format!("--stat=120,60,{MAX_STAT_FILES}"),
                base_tree,
                work_tree,
            ])
            .await?;
        let mut lines: Vec<String> = raw.lines().map(scrub_detail).collect();
        // The last line is the summary (`N files changed, ...`): it stays when the file lines are cut to the byte budget.
        let summary = lines.pop();
        Ok(bounded_lines(lines.into_iter(), MAX_STAT_BYTES, summary))
    }

    /// Every git fact of a stop in one pass: the work tree is hashed once, and a part that fails leaves the others intact. `base` is
    /// the base commit and `head` the engine's `HEAD` (object ids the caller read). Use a repository made by [`Self::facts`].
    pub async fn git_facts(&self, base: &str, head: &str) -> GitFacts {
        let mut f = GitFacts {
            progress: progress_file(&self.repo),
            ..GitFacts::default()
        };
        let mut errors: Vec<String> = Vec::new();
        let mut fail = |part: &str, e: String| errors.push(scrub_detail(&format!("{part}: {e}")));
        let trees = match self.tree_of(base).await {
            Ok(base_tree) => self.tree().await.map(|work| (base_tree, work)),
            Err(e) => Err(e),
        };
        match trees {
            Ok((base_tree, work_tree)) if base_tree != work_tree => {
                match self
                    .paths_between(&base_tree, &work_tree, MAX_FACT_PATHS)
                    .await
                {
                    Ok((paths, total)) => {
                        f.changed_files = paths;
                        f.files_total = total;
                    }
                    Err(e) => fail("changed files", e),
                }
                match self.diff_stat(&base_tree, &work_tree).await {
                    Ok(stat) if !stat.is_empty() => f.diff_stat = Some(stat),
                    Ok(_) => {}
                    Err(e) => fail("diff stat", e),
                }
            }
            Ok(_) => {}
            Err(e) => fail("work tree", e),
        }
        match self.log_since(base, head, MAX_FACT_LOG_LINES).await {
            Ok(log) if !log.is_empty() => f.git_log = Some(log),
            Ok(_) => {}
            Err(e) => fail("log", e),
        }
        f.errors = errors;
        f
    }

    /// The tree hash of the work tree as it is now (`add -A`, then `write-tree`). It is content-aware: a modified file edited
    /// again hashes differently, which `git status` does not show.
    pub async fn tree(&self) -> Result<String, String> {
        let index = self.git_dir.join("index");
        // The repository's ignore rules, as they are now, read as a regular file or not at all.
        std::fs::create_dir_all(self.git_dir.join("info")).map_err(io)?;
        let exclude =
            read_regular_nofollow(&self.repo.join(".git/info/exclude")).unwrap_or_default();
        std::fs::write(self.git_dir.join("info/exclude"), exclude).map_err(io)?;
        let envs = self.envs(&index);
        git_with_env(&self.repo, &["add", "-A"], &envs)
            .await
            .map_err(ws)?;
        git_with_env(&self.repo, &["write-tree"], &envs)
            .await
            .map_err(ws)
    }
}

#[derive(Debug)]
pub struct Snapshotter {
    scratch: ScratchRepo,
    git_dir: PathBuf,
    repo: PathBuf,
    base: String,
    last_tree: String,
    count: u32,
}

impl Snapshotter {
    /// Creates the snapshot repository for the worktree at `repo`, whose branch starts at `base`.
    pub async fn new(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        let git_dir = agent_data.join("snapshot.git");
        let scratch = ScratchRepo::open(git_dir.clone(), repo).await?;
        scratch.seed(base).await?;
        let base_tree = git_with_env(repo, &["rev-parse", &format!("{base}^{{tree}}")], &[])
            .await
            .map_err(ws)?;
        Ok(Self {
            scratch,
            git_dir,
            repo: repo.to_path_buf(),
            base: base.to_string(),
            last_tree: base_tree,
            count: 0,
        })
    }

    /// Takes a snapshot of the tree as it is now; `None` when it equals the last one.
    pub async fn snapshot(&mut self) -> Result<Option<SnapshotMade>, String> {
        let tree = self.scratch.tree().await?;
        if tree == self.last_tree {
            return Ok(None);
        }
        let gd: &Path = &self.git_dir;
        let index = self.git_dir.join("index");
        let envs: [(&str, &Path); 3] = [
            ("GIT_DIR", gd),
            ("GIT_WORK_TREE", &self.repo),
            ("GIT_INDEX_FILE", &index),
        ];
        let head = git_with_env(&self.repo, &["rev-parse", "HEAD"], &[])
            .await
            .map_err(ws)?;
        let n = self.count + 1;
        let message = format!("harness: snapshot {n}");
        let commit = git_with_env(
            &self.repo,
            &["commit-tree", &tree, "-p", &head, "-m", &message],
            &envs,
        )
        .await
        .map_err(ws)?;
        git_with_env(
            &self.repo,
            &["update-ref", "refs/heads/snapshot", &commit],
            &[("GIT_DIR", gd)],
        )
        .await
        .map_err(ws)?;
        let bundle = self.git_dir.join("snapshot.bundle");
        let _ = std::fs::remove_file(&bundle);
        git_with_env(
            &self.repo,
            &[
                "bundle",
                "create",
                "-q",
                bundle.to_str().unwrap_or_default(),
                "refs/heads/snapshot",
            ],
            &[("GIT_DIR", gd)],
        )
        .await
        .map_err(ws)?;
        self.last_tree = tree;
        self.count = n;
        Ok(Some(SnapshotMade {
            commit,
            base: self.base.clone(),
            bundle,
            count: n,
        }))
    }
}

/// `snapshot.json`, written after the bundle so a reader that sees it finds the bundle.
pub fn snapshot_json(made: &SnapshotMade, at_rfc3339: &str) -> String {
    serde_json::json!({
        "schemaVersion": 1,
        "headSha": made.commit,
        "baseSha": made.base,
        "at": at_rfc3339,
        "count": made.count,
    })
    .to_string()
}
