//! The engine's HOME holds no skills: the engine reads `$HOME/.agents/skills` besides the
//! configuration directory and the repository, so a skill left there (by an earlier attempt of the same run, whose engine owned the
//! directory) would be loaded under the run's own. HOME is a fresh directory of the run; this removes the one folder the engine reads.
//! Nothing else in HOME is touched (the package proxy's Maven settings live there), and a link is removed, never followed.

use std::io::ErrorKind;
use std::path::Path;

/// Removes `<home>/.agents/skills`. Returns whether anything was there. A link in place of `.agents` or of `skills` is unlinked and what it
/// points at is left alone; a regular file named `.agents` is not a skills folder and stays.
pub fn clear_home_skills(home: &Path) -> Result<bool, String> {
    let io = |what: &str, e: std::io::Error| format!("skills: {what}: {e}");
    let agents = home.join(".agents");
    let meta = match std::fs::symlink_metadata(&agents) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io("read the home directory", e)),
    };
    if meta.file_type().is_symlink() {
        std::fs::remove_file(&agents).map_err(|e| io("remove a link in the home directory", e))?;
        return Ok(true);
    }
    if !meta.is_dir() {
        return Ok(false);
    }
    let skills = agents.join("skills");
    let meta = match std::fs::symlink_metadata(&skills) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io("read the home skills folder", e)),
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(&skills).map_err(|e| io("remove the home skills folder", e))?;
    } else {
        std::fs::remove_file(&skills).map_err(|e| io("remove the home skills entry", e))?;
    }
    Ok(true)
}
