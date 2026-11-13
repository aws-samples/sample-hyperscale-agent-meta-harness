//! Placing the plugin folders: the worker image carries them with `lock.json` (the SHA-256 of every file); at the start of a
//! run agentd checks each file of the plugins it needs against the lock and copies them into the engine's configuration directory, which
//! belongs to root and is read-only to the engine user (files 0444, directories 0555). One file that does not match stops the placing and
//! nothing is copied: the run then goes on without the goal loop (the module logs why), never with a plugin nobody pinned.

use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// What a placing did.
#[derive(Debug, PartialEq, Eq)]
pub struct Placed {
    pub files: usize,
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The relative path when it has at least a folder and a file name and nothing else than names.
fn safe_relative(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let ok =
        p.components().count() >= 2 && p.components().all(|c| matches!(c, Component::Normal(_)));
    ok.then(|| p.to_path_buf())
}

/// Checks the files of `plugins` against `src/lock.json` and copies them to `<config_dir>/plugins/<plugin>/`.
pub fn place_plugins(src: &Path, config_dir: &Path, plugins: &[&str]) -> Result<Placed, String> {
    let lock_path = src.join("lock.json");
    let lock: serde_json::Value = std::fs::read(&lock_path)
        .map_err(|e| format!("lock.json: cannot read {}: {e}", lock_path.display()))
        .and_then(|b| {
            serde_json::from_slice(&b).map_err(|e| format!("lock.json: not JSON: {e}"))
        })?;
    if lock.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("lock.json: version is not 1".to_string());
    }
    let files = lock
        .get("files")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "lock.json: no files".to_string())?;
    // Everything is read and checked before anything is written.
    let mut verified: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for plugin in plugins {
        let mut any = false;
        for (rel, want) in files {
            if rel.split('/').next() != Some(*plugin) {
                continue;
            }
            any = true;
            let path = safe_relative(rel)
                .ok_or_else(|| format!("lock.json: {rel} is not a path inside a plugin folder"))?;
            let bytes =
                std::fs::read(src.join(&path)).map_err(|e| format!("{rel}: cannot read: {e}"))?;
            if want.as_str() != Some(hex(&bytes).as_str()) {
                return Err(format!("{rel} does not match lock.json"));
            }
            verified.push((path, bytes));
        }
        if !any {
            return Err(format!("lock.json has no file of plugin {plugin}"));
        }
    }
    let dest = config_dir.join("plugins");
    let same = verified
        .iter()
        .all(|(rel, bytes)| std::fs::read(dest.join(rel)).is_ok_and(|b| &b == bytes));
    if !same {
        for (rel, bytes) in &verified {
            let file = dest.join(rel);
            let dir = file.parent().unwrap_or(&dest);
            // An earlier placing left read-only directories; the configuration directory belongs to this process.
            for d in [dir, dest.as_path()] {
                if d.is_dir() {
                    set_mode(d, 0o755).map_err(|e| format!("{}: {e}", d.display()))?;
                }
            }
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            if file.exists() {
                std::fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            }
            std::fs::write(&file, bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        }
        for (rel, _) in &verified {
            let file = dest.join(rel);
            set_mode(&file, 0o444).map_err(|e| format!("{}: {e}", file.display()))?;
            crate::engine::direct::own_by_root(&file)
                .map_err(|e| format!("{}: {e}", file.display()))?;
        }
    }
    // Directories last, from the leaves up, so a second placing can still change what it must.
    let mut dirs: Vec<PathBuf> = verified
        .iter()
        .filter_map(|(rel, _)| dest.join(rel).parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs.push(dest.clone());
    for d in dirs {
        set_mode(&d, 0o555).map_err(|e| format!("{}: {e}", d.display()))?;
        crate::engine::direct::own_by_root(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    Ok(Placed {
        files: verified.len(),
    })
}
