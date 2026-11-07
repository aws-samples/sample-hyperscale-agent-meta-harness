//! The skills preflight: before any model call agentd asks
//! the engine which skills it will load, twice.
//!
//! - Before the run's skills are placed, [`check_repo_skills`]: the listing shows only the repository's and the home directory's skills. A
//!   repository skill with the name of a requested one would be replaced silently (the configuration directory wins a name clash on the
//!   pinned engine), so the run stops and names the file. A skill from anywhere but the repository stops it too.
//! - After they are placed, [`check_placed_listing`]: every placed skill is listed once, at its own file, reading the way the file reads, the
//!   file on the disk is still the file placed, and nothing else is loaded from outside the repository.
//!
//! The configuration directory's skills folder may hold skills of other modules (the research module places its context there): a skill
//! listed from inside that folder is not a foreign skill ([`check_repo_skills_in`], [`check_placed_listing_in`]), unless a requested skill has
//! its name. What an earlier attempt of this run placed is retired before the first listing, so the first listing shows no skill of the run.
//!
//! A failure ends the run `stopped_setup_failed`. The messages name the rule, the skill and a path, never the instructions.

use crate::direct::classify::scrub_detail;
use crate::direct::request::sha256_hex;
use crate::direct::skills::place::PlacedSkill;
use crate::engine::LaunchSpec;
use crate::engine::direct::{ListedSkill, parse_skills_listing};
use harness_protocol::direct::{SkillError, SkillProblem};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Where the file a row names is, relative to `repo`: `Some(path inside the repository)` or `None` when it is outside. A path that resolves
/// is judged by where it resolves to (a link out of the repository is outside); one that does not resolve is judged by its words, and is an
/// error when the words put it inside (it cannot be shown to be a file).
fn locate(path: &str, repo: &Path) -> Result<Option<String>, String> {
    if path.is_empty() {
        return Err("it names no file".to_string());
    }
    let repo_real = repo.canonicalize().unwrap_or_else(|_| lexical(repo));
    match Path::new(path).canonicalize() {
        Ok(real) => Ok(real
            .strip_prefix(&repo_real)
            .ok()
            .map(|p| p.display().to_string())),
        Err(_) => {
            let words = lexical(Path::new(path));
            if words.starts_with(&repo_real) || words.starts_with(lexical(repo)) {
                Err("the file it names cannot be read".to_string())
            } else {
                Ok(None)
            }
        }
    }
}

fn outside(name: &str, path: &str) -> String {
    format!(
        "skills preflight: the skill {name} is loaded from outside the repository ({})",
        scrub_detail(path)
    )
}

/// Whether `path` is a file inside `dir` (both resolved: a link out of `dir` is outside).
fn inside(path: &str, dir: &Path) -> bool {
    match (Path::new(path).canonicalize(), dir.canonicalize()) {
        (Ok(real), Ok(base)) => real.starts_with(base),
        _ => false,
    }
}

/// The listing taken before the run's skills are placed. `requested` are the names of the run's skills.
pub fn check_repo_skills(
    rows: &[ListedSkill],
    repo: &Path,
    requested: &[String],
) -> Result<(), String> {
    check_repo_skills_in(rows, repo, requested, None)
}

/// [`check_repo_skills`] for a configuration directory whose `skills_dir` may hold skills of other modules: a skill listed from inside
/// `skills_dir` is not a foreign skill and stays, unless a requested skill has its name (the same "already there" words as for a
/// repository skill).
pub fn check_repo_skills_in(
    rows: &[ListedSkill],
    repo: &Path,
    requested: &[String],
    skills_dir: Option<&Path>,
) -> Result<(), String> {
    for row in rows {
        let taken = requested.iter().any(|n| n.eq_ignore_ascii_case(&row.name));
        let relative = match locate(&row.path, repo) {
            Ok(Some(rel)) => rel,
            Ok(None) => match skills_dir {
                Some(dir) if inside(&row.path, dir) => {
                    if taken {
                        let at = Path::new(&row.path)
                            .strip_prefix(dir)
                            .map_or_else(|_| row.path.clone(), |p| p.display().to_string());
                        return Err(shadowed(&row.name, at));
                    }
                    continue;
                }
                _ => return Err(outside(&row.name, &row.path)),
            },
            Err(why) => {
                return Err(format!(
                    "skills preflight: the skill {} cannot be placed in the repository: {why}",
                    row.name
                ));
            }
        };
        if taken {
            return Err(shadowed(&row.name, relative));
        }
    }
    Ok(())
}

fn shadowed(name: &str, file: String) -> String {
    let e = SkillError {
        skill: Some(name.to_string()),
        file: Some(file),
        problem: SkillProblem::Shadowed,
    };
    format!("skills preflight: {e}")
}

fn same_file(a: &str, b: &Path) -> bool {
    if Path::new(a) == b {
        return true;
    }
    match (Path::new(a).canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The listing taken after the run's skills are placed.
pub fn check_placed_listing(
    rows: &[ListedSkill],
    placed: &[PlacedSkill],
    repo: &Path,
) -> Result<(), String> {
    check_placed_listing_in(rows, placed, repo, None)
}

/// [`check_placed_listing`] with the same allowance for the skills of other modules in `skills_dir`.
pub fn check_placed_listing_in(
    rows: &[ListedSkill],
    placed: &[PlacedSkill],
    repo: &Path,
    skills_dir: Option<&Path>,
) -> Result<(), String> {
    for p in placed {
        let name = &p.name;
        // What is on the disk is what was placed.
        for f in &p.files {
            let on_disk = std::fs::read(p.dir.join(&f.path)).ok();
            if on_disk.is_none_or(|b| sha256_hex(&b) != f.sha256) {
                return Err(format!(
                    "skills preflight: the file {} of {name} changed on disk after it was placed",
                    f.path
                ));
            }
        }
        let named: Vec<&ListedSkill> = rows.iter().filter(|r| &r.name == name).collect();
        let row = match named.as_slice() {
            [] => {
                return Err(format!(
                    "skills preflight: {name} is missing from the skills the engine will load"
                ));
            }
            [one] => *one,
            _ => {
                return Err(format!("skills preflight: {name} is listed more than once"));
            }
        };
        if !same_file(&row.path, &p.skill_md) {
            return Err(format!(
                "skills preflight: {name} is shadowed by a skill of the same name loaded from {}",
                scrub_detail(&row.path)
            ));
        }
        if row.frontmatter.get("name").and_then(|v| v.as_str()) != Some(name.as_str()) {
            return Err(format!(
                "skills preflight: {name} is listed with a frontmatter name that is not its folder name"
            ));
        }
        if row
            .description
            .as_deref()
            .is_none_or(|d| d.trim().is_empty())
        {
            return Err(format!(
                "skills preflight: {name} is listed with no description"
            ));
        }
        if sha256_hex(row.instructions.trim().as_bytes()) != sha256_hex(p.body.as_bytes()) {
            return Err(format!(
                "skills preflight: {name} differs from the file placed"
            ));
        }
    }
    for row in rows {
        if placed.iter().any(|p| p.name == row.name) {
            continue;
        }
        match locate(&row.path, repo) {
            Ok(Some(_)) => {}
            Ok(None) => match skills_dir {
                Some(dir) if inside(&row.path, dir) => {}
                _ => return Err(outside(&row.name, &row.path)),
            },
            Err(why) => {
                return Err(format!(
                    "skills preflight: the skill {} cannot be placed in the repository: {why}",
                    row.name
                ));
            }
        }
    }
    Ok(())
}

/// Runs the listing command as launched (`spec`), cut off after `limit`, and returns its rows.
pub async fn run_skills_listing(
    spec: &LaunchSpec,
    limit: Duration,
) -> Result<Vec<ListedSkill>, String> {
    let argv = crate::engine::command_argv(spec).map_err(|e| format!("skills preflight: {e}"))?;
    let joined = std::env::join_paths(&spec.path).map_err(|e| format!("skills preflight: {e}"))?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let secs = limit.as_secs();
    let out = tokio::time::timeout(limit, cmd.output())
        .await
        .map_err(|_| format!("skills preflight: the listing did not finish in {secs} s"))?
        .map_err(|e| format!("skills preflight: the listing did not start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "skills preflight: the listing exited with {}",
            out.status
        ));
    }
    parse_skills_listing(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| format!("skills preflight: {e}"))
}
