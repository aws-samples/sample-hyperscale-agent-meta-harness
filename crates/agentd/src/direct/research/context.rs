//! `research::context`: the read-only home of the material a run fetches, inside the context directory
//! of the fresh session (`crate::direct::context::ContextDir`, `<agent data>/context/`, outside the work tree, so nothing in it can reach
//! the end commit, the snapshots, the zip or the pull request).
//!
//! ```text
//! context/research/                 0755, made by agentd (root in a microVM); agentd adds to it, the engine reads it
//!   INDEX.json  INDEX.md            0444, written again after each placement, derived from the manifests
//!   pages/<item>.txt                0444, a stored page as the route cleaned it
//!   repos/<owner>__<repo>__<commit12>/...   0555 directories, 0444 files, the repository as the route filtered it
//!   manifests/<item>.json           0444, the route's manifest, byte for byte
//!   repos/.stage-<item>-<n>         an unpack in progress, renamed into place (same parent, so a sealed directory can move); never left
//! ```
//!
//! The directory belongs to the user that runs agentd. In a microVM that is root and the engine is another user, so the engine can read
//! every file and change none. (The engine could still ask for an item through the API; it cannot write one.)

use crate::direct::context::ContextDir;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const RESEARCH_DIR: &str = "research";
pub const PAGES_DIR: &str = "pages";
pub const REPOS_DIR: &str = "repos";
pub const MANIFESTS_DIR: &str = "manifests";
/// An unpack in progress is a directory of `repos/` with this name prefix.
pub const STAGE_PREFIX: &str = ".stage-";
pub const INDEX_JSON: &str = "INDEX.json";
pub const INDEX_MD: &str = "INDEX.md";

/// An item id is a plain name: 1 to 32 characters of letters, digits, `_` and `-`, starting with a letter or a digit.
pub fn valid_item_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

/// `owner__repo__<first 12 characters of the commit>` for a GitHub repository URL (`https://github.com/<owner>/<repo>[.git]`), without
/// the commit part when the commit is `None`. `None` for any other origin, for a name that is not plain, and for a commit that is not
/// at least 12 hexadecimal characters: the caller then names the directory after the item.
pub fn repo_dir_name(origin: &str, commit: Option<&str>) -> Option<String> {
    let rest = origin.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let plain = |s: &str| {
        (1..=100).contains(&s.len())
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    };
    if !plain(owner) || !plain(repo) {
        return None;
    }
    match commit {
        None => Some(format!("{owner}__{repo}")),
        Some(c) if c.len() >= 12 && c.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(format!("{owner}__{repo}__{}", c[..12].to_ascii_lowercase()))
        }
        Some(_) => None,
    }
}

fn io(e: &std::io::Error) -> String {
    format!("research context: {}", e.kind())
}

/// `<agent data>/context/research/` and what lives in it.
#[derive(Clone, Debug)]
pub struct ResearchDir {
    root: PathBuf,
}

impl ResearchDir {
    /// Makes the directory and its three subdirectories (0755). A link or a file where one belongs is refused. A staging directory left
    /// by an earlier run of agentd is removed.
    pub fn create(context: &ContextDir) -> Result<Self, String> {
        let root = context.path().join(RESEARCH_DIR);
        ensure_dir(&root)?;
        for sub in [PAGES_DIR, REPOS_DIR, MANIFESTS_DIR] {
            ensure_dir(&root.join(sub))?;
        }
        // An unpack that an earlier agentd left unfinished.
        if let Ok(rd) = std::fs::read_dir(root.join(REPOS_DIR)) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(STAGE_PREFIX) {
                    let path = e.path();
                    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir()) {
                        crate::direct::research::extract::remove_tree(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn page_path(&self, item_id: &str) -> PathBuf {
        self.root.join(PAGES_DIR).join(format!("{item_id}.txt"))
    }

    pub fn manifest_path(&self, item_id: &str) -> PathBuf {
        self.root
            .join(MANIFESTS_DIR)
            .join(format!("{item_id}.json"))
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join(REPOS_DIR)
    }

    /// Where an unpack of `item_id` happens (`n` makes the name new for each try). It is a directory of `repos/`, so the finished tree
    /// is renamed within one parent: a directory that is sealed read-only can be moved that way by a user who is not root.
    pub fn stage_path(&self, item_id: &str, n: u64) -> PathBuf {
        self.repos_dir()
            .join(format!("{STAGE_PREFIX}{item_id}-{n}"))
    }

    /// Writes `bytes` to `path` (0444) through a temporary name in the same directory that is never followed if it is a link, then
    /// renames it over the final name, so a reader sees the old file or the new one and never half of one.
    pub fn write_readonly(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        let name = path
            .file_name()
            .ok_or_else(|| "research context: no file name".to_string())?
            .to_string_lossy()
            .into_owned();
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&e)),
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&tmp)
            .map_err(|e| io(&e))?;
        file.write_all(bytes).map_err(|e| io(&e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o444))
            .map_err(|e| io(&e))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| io(&e))
    }
}

/// A directory that exists with mode 0755 after the call; a link or a file of that name is refused.
fn ensure_dir(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "research context: {} exists and is not a directory",
                dir.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dir).map_err(|e| io(&e))?;
        }
        Err(e) => return Err(io(&e)),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).map_err(|e| io(&e))
}
