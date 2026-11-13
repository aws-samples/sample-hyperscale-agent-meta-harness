//! Reading the user's skills from disk. A skill is a folder: `SKILL.md` (frontmatter with `name` and `description`, then the
//! instructions) and optional scripts and reference files. `ham` reads `~/.config/ham/skills/<name>/...` (beside `rules/`), or the folder
//! `--skills <path>` names, and sends the files as the `skills` member of the start request.
//!
//! The shared validator (`harness_protocol::direct::check_skills`) judges the wire form; the checks that need the disk are here:
//!
//! - a symbolic link, a device or a directory where a file belongs, inside a skill, is refused (what is sent must be what the folder holds);
//!   the skills folder and a skill folder may themselves be links, because a dotfile manager makes them;
//! - a file has no size of its own and a skill no count of files, but a file longer than a whole request can be
//!   (`harness_protocol::direct::MAX_REQUEST_BYTES`), or skills that together are, are refused before they are read, with the bound named: they
//!   could never be sent. A name that is not UTF-8 or text that is not UTF-8 is an error;
//! - the frontmatter of `SKILL.md` is read by the shared reader (`harness_protocol::direct::skill_document`) that `check_skills` calls: `name`
//!   is the folder name, `description` has text and is at most 1,024 characters, and the body is not empty;
//! - hidden entries (`.DS_Store`, editor swap files, `.git`) are skipped, as they are for rules;
//! - the namespace of the platform's skills is reserved: the lifecycle pack's skills are `lifecycle-<step>`, so a skill of the
//!   user's own may not take another name that starts with `lifecycle-` ([`check_namespace`]). The pack's seven names stay free to use, also
//!   with the user's edits. The shared validator is part of the worker image, so this half of the rule is the client's.

use crate::config::config_path;
use crate::pack;
use harness_protocol::direct::{
    MAX_REQUEST_BYTES, MAX_SKILL_PATH_BYTES, SKILL_ENTRY, Skill, SkillFile, SkillProblem,
    check_skills, exceeds_request, skill_sha256,
};
use std::fmt;
use std::path::{Path, PathBuf};

/// The folder beside `config.toml` and `rules/`.
pub const SKILLS_DIR_NAME: &str = "skills";

/// What is wrong with a skill or the folder of skills.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// A rule of the shared validator, or a disk check that uses its words (`NotRegular`).
    Invalid(SkillProblem),
    /// A file that is not UTF-8 text.
    NotUtf8,
    /// The operating system's reason.
    Unreadable(String),
    /// An entry of the skills folder that is not a skill folder.
    NotASkill,
    /// A name inside the namespace the platform keeps for its own skills (`lifecycle-` and not one of the pack's seven).
    Reserved,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Invalid(p) => write!(f, "{p}"),
            Why::NotUtf8 => write!(f, "is not UTF-8 text"),
            Why::Unreadable(e) => write!(f, "cannot be read: {e}"),
            Why::NotASkill => write!(
                f,
                "is not a skill folder (the skills folder holds one folder per skill, each with a SKILL.md)"
            ),
            Why::Reserved => write!(
                f,
                "is in the namespace reserved for the platform's own skills: names that start with {:?} belong to the lifecycle pack ({}); give your skill another name (rename its folder and the name line of its SKILL.md)",
                pack::SKILL_PREFIX,
                pack::skill_names().join(", ")
            ),
        }
    }
}

/// A problem, with the skill and the file it concerns (names only; never content).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillsError {
    pub skill: Option<String>,
    pub file: Option<String>,
    pub why: Why,
}

impl fmt::Display for SkillsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.skill, &self.file) {
            (Some(s), Some(file)) => write!(f, "skill {s:?} file {file:?} {}", self.why),
            (Some(s), None) => write!(f, "skill {s:?} {}", self.why),
            (None, Some(file)) => write!(f, "skills: {file:?} {}", self.why),
            (None, None) => write!(f, "skills: {}", self.why),
        }
    }
}

impl std::error::Error for SkillsError {}

fn error(skill: Option<&str>, file: Option<&str>, why: Why) -> SkillsError {
    SkillsError {
        skill: skill.map(str::to_string),
        file: file.map(str::to_string),
        why,
    }
}

/// `~/.config/ham/skills` (or under `XDG_CONFIG_HOME`), beside `config.toml`; `None` without `HOME`.
pub fn skills_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    config_path(env).map(|p| p.with_file_name(SKILLS_DIR_NAME))
}

/// The skills of the default folder; no folder (or no `HOME`) is no skills.
pub fn read_default(env: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Skill>, SkillsError> {
    match skills_dir(env) {
        Some(dir) if dir.exists() => read_skills_path(&dir),
        _ => Ok(Vec::new()),
    }
}

fn unreadable(skill: Option<&str>, file: Option<&str>, e: &std::io::Error) -> SkillsError {
    error(skill, file, Why::Unreadable(e.to_string()))
}

/// The skills at `path`: a folder of skill folders (sorted by name), or one skill folder (it holds `SKILL.md`), whose name is the
/// folder's. Validated as a set. An empty folder is no skills.
pub fn read_skills_path(path: &Path) -> Result<Vec<Skill>, SkillsError> {
    let shown = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|e| unreadable(None, Some(&shown), &e))?;
    if !meta.is_dir() {
        return Err(error(
            None,
            Some(&shown),
            Why::Unreadable("is not a folder".into()),
        ));
    }
    let mut skills = Vec::new();
    // The bytes of all the files read so far: the files of the skills together must be sendable in one request.
    let mut total: u64 = 0;
    if path.join(SKILL_ENTRY).is_file() {
        let folder = std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .ok_or_else(|| error(None, Some(&shown), Why::NotASkill))?;
        skills.push(read_skill(&folder, path, &mut total)?);
    } else {
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(path)
            .map_err(|e| unreadable(None, Some(&shown), &e))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| unreadable(None, Some(&shown), &e))?;
        names.sort();
        for name in names {
            let lossy = name.to_string_lossy().into_owned();
            if lossy.starts_with('.') {
                continue;
            }
            // A link to a skill folder is followed (a dotfile manager makes them); anything that is not a folder is stray.
            let is_dir = std::fs::metadata(path.join(&name)).is_ok_and(|m| m.is_dir());
            if !is_dir {
                return Err(error(None, Some(&lossy), Why::NotASkill));
            }
            skills.push(read_skill(&lossy, &path.join(&name), &mut total)?);
        }
    }
    check_skills(&skills).map_err(|e| SkillsError {
        skill: e.skill,
        file: e.file,
        why: Why::Invalid(e.problem),
    })?;
    check_namespace(&skills)?;
    Ok(skills)
}

/// The namespace of the platform's skills. The lifecycle pack names its skills `lifecycle-<step>` ([`pack::SKILL_PREFIX`]); that
/// prefix is reserved, so a skill of the user's own cannot be taken for one of the platform's. A name outside the prefix is the user's; the
/// pack's seven names are allowed (they are the pack's skills, with or without the user's edits to their files); any other name that starts
/// with the prefix is refused, naming the skill. Pure: no disk is read.
pub fn check_namespace(skills: &[Skill]) -> Result<(), SkillsError> {
    match skills
        .iter()
        .find(|s| s.name.starts_with(pack::SKILL_PREFIX) && !pack::is_skill_name(&s.name))
    {
        Some(s) => Err(error(Some(&s.name), None, Why::Reserved)),
        None => Ok(()),
    }
}

/// One skill folder: every visible regular file, `SKILL.md` first and the rest by path.
fn read_skill(name: &str, dir: &Path, total: &mut u64) -> Result<Skill, SkillsError> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    walk(name, dir, "", &mut found)?;
    found.sort_by(|a, b| {
        (a.0 != SKILL_ENTRY, a.0.as_str()).cmp(&(b.0 != SKILL_ENTRY, b.0.as_str()))
    });
    let mut files = Vec::new();
    for (rel, full) in &found {
        let meta = std::fs::metadata(full).map_err(|e| unreadable(Some(name), Some(rel), &e))?;
        if exceeds_request(meta.len()) {
            return Err(error(
                Some(name),
                Some(rel),
                Why::Invalid(SkillProblem::TooLarge {
                    bytes: usize::try_from(meta.len()).unwrap_or(usize::MAX),
                    max: MAX_REQUEST_BYTES,
                }),
            ));
        }
        *total = total.saturating_add(meta.len());
        if exceeds_request(*total) {
            return Err(error(
                Some(name),
                Some(rel),
                Why::Invalid(SkillProblem::TooLargeTotal {
                    bytes: usize::try_from(*total).unwrap_or(usize::MAX),
                    max: MAX_REQUEST_BYTES,
                }),
            ));
        }
        let bytes = std::fs::read(full).map_err(|e| unreadable(Some(name), Some(rel), &e))?;
        let text =
            String::from_utf8(bytes).map_err(|_| error(Some(name), Some(rel), Why::NotUtf8))?;
        files.push(SkillFile::new(rel, &text));
    }
    Ok(Skill {
        name: name.to_string(),
        files,
    })
}

/// Collects `(relative path, full path)` of the regular files below `dir`; a link or a special file is an error.
fn walk(
    skill: &str,
    dir: &Path,
    rel: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), SkillsError> {
    let mut entries: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
        .map_err(|e| unreadable(Some(skill), Some(rel), &e))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()
        .map_err(|e| unreadable(Some(skill), Some(rel), &e))?;
    entries.sort();
    for name in entries {
        let lossy = name.to_string_lossy().into_owned();
        if lossy.starts_with('.') {
            continue;
        }
        let shown = if rel.is_empty() {
            lossy.clone()
        } else {
            format!("{rel}/{lossy}")
        };
        // The path is held to its length (not to a depth): a folder tree below the longest path a skill may carry is refused before it is walked.
        if shown.len() > MAX_SKILL_PATH_BYTES {
            return Err(error(
                Some(skill),
                Some(&shown),
                Why::Invalid(SkillProblem::BadPath),
            ));
        }
        let full = dir.join(&name);
        let meta = std::fs::symlink_metadata(&full)
            .map_err(|e| unreadable(Some(skill), Some(&shown), &e))?;
        let kind = meta.file_type();
        if kind.is_dir() {
            walk(skill, &full, &shown, out)?;
        } else if kind.is_file() {
            out.push((shown, full));
        } else {
            return Err(error(
                Some(skill),
                Some(&shown),
                Why::Invalid(SkillProblem::NotRegular),
            ));
        }
    }
    Ok(())
}

/// One line per skill for the start of a run: name, the first 12 digits of its digest, files and bytes.
pub fn describe(skills: &[Skill]) -> Vec<String> {
    skills
        .iter()
        .map(|s| {
            let bytes: u64 = s.files.iter().map(|f| f.size).sum();
            let n = s.files.len();
            format!(
                "{} sha256 {} ({n} file{}, {bytes} bytes)",
                s.name,
                &skill_sha256(s)[..12],
                if n == 1 { "" } else { "s" }
            )
        })
        .collect()
}
