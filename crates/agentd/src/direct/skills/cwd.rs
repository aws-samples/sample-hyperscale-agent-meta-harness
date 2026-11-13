//! The session directory: the engine loads a repository's AGENTS.md and rule files only when its
//! session starts in the repository's git top level (below it skills still load and the rest vanishes), so the session starts there. The
//! driver's own default is the same directory; the skills module sets it again so the guarantee does not depend on that default.

use std::path::Path;

/// `git rev-parse --show-toplevel` of `dir`, with agentd's git guards (no hooks, no configuration of the system or the user).
pub async fn top_level(dir: &Path) -> Result<String, String> {
    crate::workspace::git_with_env(dir, &["rev-parse", "--show-toplevel"], &[])
        .await
        .map_err(|e| format!("skills: the session directory: {e}"))
}
