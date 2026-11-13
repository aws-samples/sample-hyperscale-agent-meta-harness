//! The file-placement check: the check of the rules and skills an engine will load for an engine that has no listing command
//! ([`crate::engine::layout::PlacementCheck::Files`]). The engine's own listing is the stronger check where there is one (`crate::direct::preflight`
//! for rules, `crate::direct::skills::preflight` for skills); this one reads the disk: every placed file is there with the bytes that were
//! placed and is not writable, nothing else is in the rules directory, and the repository holds no file or folder of the same name where the
//! engine would read it instead. A failure ends the run `stopped_setup_failed` before any model call. The messages name the rule or skill and
//! never its text.

use crate::direct::request::sha256_hex;
use crate::direct::rules_place::PlacedRule;
use crate::direct::skills::place::PlacedSkill;
use crate::engine::layout::{ConfigLayout, repo_rule_dirs, repo_skill_dirs};
use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// `Ok` when `dir` is a directory and no link; the error names `what`.
fn plain_dir(dir: &Path, what: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "{what} placement: the {what} directory {} is a symbolic link",
            dir.display()
        )),
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "{what} placement: {} is not a directory",
            dir.display()
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            Err(format!("{what} placement: the {what} directory is missing"))
        }
        Err(e) => Err(format!("{what} placement: {e}")),
    }
}

/// Reads a placed file; the error says whether it is missing or cannot be read.
fn read_placed(path: &Path, what: &str, label: &str) -> Result<Vec<u8>, String> {
    // A link that leads to the same bytes is not the file that was placed: the engine would read whatever the link later points at.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{what} placement: {label} is a symbolic link, not the file that was placed"
        ));
    }
    std::fs::read(path).map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            format!("{what} placement: {label} is missing from the {what} directory")
        } else {
            format!("{what} placement: {label} cannot be read: {e}")
        }
    })
}

/// Checks the rules `place_rules` wrote into `layout.rules` for an engine that cannot list them.
pub fn verify_rules_placed(
    engine: &str,
    layout: &ConfigLayout,
    placed: &[PlacedRule],
    repo: &Path,
) -> Result<(), String> {
    plain_dir(&layout.rules, "rules")?;
    for p in placed {
        let bytes = read_placed(&layout.rules.join(&p.name), "rules", &p.name)?;
        if sha256_hex(&bytes) != p.sha256 {
            return Err(format!(
                "rules placement: {} differs from the file placed",
                p.name
            ));
        }
        let mode = std::fs::metadata(layout.rules.join(&p.name))
            .map_err(|e| format!("rules placement: {}: {e}", p.name))?
            .permissions()
            .mode();
        if mode & 0o222 != 0 {
            return Err(format!(
                "rules placement: {} is writable, so the engine could rewrite it",
                p.name
            ));
        }
    }
    let expected: BTreeSet<&str> = placed.iter().map(|p| p.name.as_str()).collect();
    let entries = std::fs::read_dir(&layout.rules)
        .map_err(|e| format!("rules placement: the rules directory cannot be read: {e}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !expected.contains(name.as_str()) {
            return Err(format!(
                "rules placement: unexpected entry {name} in the rules directory"
            ));
        }
    }
    for d in repo_rule_dirs(engine) {
        for p in placed {
            let stem = p.name.strip_suffix(".md").unwrap_or(&p.name);
            for clash in [p.name.as_str(), stem] {
                if std::fs::symlink_metadata(repo.join(d).join(clash)).is_ok() {
                    return Err(format!(
                        "rules placement: {} is shadowed by the repository's {d}/{clash}",
                        p.name
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Checks the skills `place_skills` wrote into `layout.skills` for an engine that cannot list them.
pub fn verify_skills_placed(
    engine: &str,
    layout: &ConfigLayout,
    placed: &[PlacedSkill],
    repo: &Path,
) -> Result<(), String> {
    plain_dir(&layout.skills, "skills")?;
    for p in placed {
        for f in &p.files {
            let label = format!("{}/{}", p.name, f.path);
            let bytes = read_placed(&p.dir.join(&f.path), "skills", &label)?;
            if sha256_hex(&bytes) != f.sha256 {
                return Err(format!(
                    "skills placement: {label} differs from the file placed"
                ));
            }
        }
        for d in repo_skill_dirs(engine) {
            if std::fs::symlink_metadata(repo.join(d).join(&p.name)).is_ok() {
                return Err(format!(
                    "skills placement: {} is shadowed by the repository's {d}/{}",
                    p.name, p.name
                ));
            }
        }
    }
    Ok(())
}
