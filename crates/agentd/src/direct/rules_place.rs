//! Places the user's rules in the engine's global rules directory: each file is the exact
//! bytes of `RuleFile.content`, mode 0444, in a directory of mode 0555, and nothing else is in that directory. agentd runs as
//! root after it gave the engine user its own directories (`chown`), so the files and the directory belong to root and the
//! engine can neither rewrite a rule nor add one. The engine loads the directory itself; no rule is copied into the prompt.

use crate::direct::request::{sha256_hex, valid_rule_name};
use harness_protocol::api::v1::RuleFile;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A rule file as placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedRule {
    pub name: String,
    /// SHA-256 of the file's bytes.
    pub sha256: String,
    /// SHA-256 of the file's text trimmed (the engine's rules listing trims).
    pub text_sha256: String,
    pub bytes: u64,
    pub path: PathBuf,
}

/// Writes `rules` into `dir` and locks it. With no rules nothing is created. A directory an earlier attempt left read-only is
/// replaced.
pub fn place_rules(dir: &Path, rules: &[RuleFile]) -> Result<Vec<PlacedRule>, String> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    // Names were checked with the request; a name that could leave the directory is refused again here, where it would act.
    if let Some(bad) = rules.iter().find(|r| !valid_rule_name(&r.path)) {
        return Err(format!(
            "rules: {:?} is not a flat ASCII .md name",
            crate::direct::classify::scrub_detail(&bad.path)
        ));
    }
    let io = |what: &str, e: std::io::Error| format!("rules: {what}: {e}");
    crate::engine::direct::refuse_symlink(dir).map_err(|e| format!("rules: {e}"))?;
    if dir.exists() {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io("unlock the directory", e))?;
        std::fs::remove_dir_all(dir).map_err(|e| io("clear the directory", e))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| io("create the directory", e))?;
    let mut placed = Vec::with_capacity(rules.len());
    for r in rules {
        let path = dir.join(&r.path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
            .map_err(|e| io(&format!("write {}", r.path), e))?;
        f.write_all(r.content.as_bytes())
            .map_err(|e| io(&format!("write {}", r.path), e))?;
        drop(f);
        // The creation mode is cut by the umask; the final mode is set outright.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| io(&format!("lock {}", r.path), e))?;
        crate::engine::direct::own_by_root(&path).map_err(|e| io(&format!("own {}", r.path), e))?;
        placed.push(PlacedRule {
            name: r.path.clone(),
            sha256: sha256_hex(r.content.as_bytes()),
            text_sha256: sha256_hex(r.content.trim().as_bytes()),
            bytes: r.content.len() as u64,
            path,
        });
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))
        .map_err(|e| io("lock the directory", e))?;
    crate::engine::direct::own_by_root(dir).map_err(|e| io("own the directory", e))?;
    Ok(placed)
}
