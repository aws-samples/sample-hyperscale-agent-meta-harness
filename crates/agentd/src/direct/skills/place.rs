//! Places the run's skills where the engine reads them: `<config dir>/skills/<name>/...`, each file the exact bytes the
//! client sent, mode 0444, in directories of mode 0555, all owned by root, so the engine can read a skill and can neither rewrite one nor add
//! one. The directory sits in the engine's configuration directory, outside the repository. The wire rules of the request are
//! `harness_protocol::direct::check_skills`; what is checked again here is what acts on the disk: that a path stays inside its skill's
//! folder, and that the bytes about to be written are the bytes the hash and size name.
//!
//! agentd owns the folders it places and nothing else in the skills directory:
//! every folder it writes holds the marker file [`MARKER`], written first, and only a folder that holds it is replaced
//! or removed by [`retire_placed`] and [`place_skills`]. A second invocation of the same run on the same VM thus replaces what the first
//! attempt placed. A folder of a requested name that agentd did not place is a clash and is refused; it is never deleted.

use crate::direct::request::sha256_hex;
use harness_protocol::direct::{SKILL_ENTRY, Skill, SkillError, SkillProblem, skill_sha256};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub use harness_protocol::direct::SkillDoc;

/// The marker file inside every skill folder agentd placed: only a folder that holds it is agentd's to replace or remove. A skill's own
/// files can never start with a dot, so it cannot clash with one.
pub const MARKER: &str = ".agentd-placed";

/// A file of a skill as placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedFile {
    /// Relative to the skill's folder, `/`-separated.
    pub path: String,
    /// SHA-256 of the file's bytes.
    pub sha256: String,
    pub bytes: u64,
}

/// A skill as placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedSkill {
    pub name: String,
    /// `<skills dir>/<name>`.
    pub dir: PathBuf,
    /// `<skills dir>/<name>/SKILL.md`, the file the engine lists.
    pub skill_md: PathBuf,
    /// The text after the frontmatter of SKILL.md, trimmed at both ends: what the engine's listing calls `instructions`.
    pub body: String,
    /// `harness_protocol::direct::skill_sha256` of the skill: the same digest `ham` and control-api compute.
    pub digest: String,
    pub files: Vec<PlacedFile>,
}

/// Reads a SKILL.md the way the shared validator reads it (`harness_protocol::direct::skill_document`: the one reader of the request and of
/// this module, so a text accepted at the request is accepted here). `name` must equal `folder`, `description` must have text and the
/// instructions must not be empty. The messages name the rule, never the content.
pub fn skill_document(text: &str, folder: &str) -> Result<SkillDoc, String> {
    harness_protocol::direct::skill_document(text, folder)
        .map_err(|reason| format!("SKILL.md {reason}"))
}

/// A path part that is safe to join: not empty, not `.` or `..`, no separator or NUL.
fn plain_part(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\', '\0'])
}

fn problem(skill: &str, file: Option<&str>, problem: SkillProblem) -> String {
    SkillError {
        skill: Some(skill.to_string()),
        file: file.map(String::from),
        problem,
    }
    .to_string()
}

/// Checks what acts on the disk, before anything is written: the name and every path are plain parts, and the content is the content the
/// hash and the size name. Returns each skill's parsed SKILL.md.
fn check_before_writing(skills: &[Skill]) -> Result<Vec<SkillDoc>, String> {
    let mut docs = Vec::with_capacity(skills.len());
    for s in skills {
        if !plain_part(&s.name) {
            return Err(problem(&s.name, None, SkillProblem::BadName));
        }
        let mut entry = None;
        for f in &s.files {
            if f.path.split('/').any(|part| !plain_part(part)) {
                return Err(problem(&s.name, Some(&f.path), SkillProblem::BadPath));
            }
            if f.content.len() as u64 != f.size {
                return Err(problem(&s.name, Some(&f.path), SkillProblem::SizeMismatch));
            }
            if sha256_hex(f.content.as_bytes()) != f.sha256 {
                return Err(problem(&s.name, Some(&f.path), SkillProblem::ShaMismatch));
            }
            if f.path == SKILL_ENTRY {
                entry = Some(f);
            }
        }
        let entry = entry.ok_or_else(|| problem(&s.name, None, SkillProblem::MissingEntry))?;
        docs.push(
            skill_document(&entry.content, &s.name)
                .map_err(|e| format!("skill {:?}: {e}", s.name))?,
        );
    }
    Ok(docs)
}

/// Gives every directory below `dir` (and `dir`) its write bit back so a tree an earlier attempt locked can be removed. Links are never
/// followed.
fn unlock_tree(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Ok(());
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    for entry in std::fs::read_dir(dir)? {
        unlock_tree(&entry?.path())?;
    }
    Ok(())
}

fn dirs_below(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    out.push(dir.to_path_buf());
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if std::fs::symlink_metadata(&path)?.is_dir() {
            dirs_below(&path, out)?;
        }
    }
    Ok(())
}

/// Whether `dir` is a folder agentd placed: a real directory (not a link) that holds the regular file [`MARKER`] (not a link).
fn placed_by_agentd(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(dir.join(MARKER)).is_ok_and(|m| m.is_file())
}

/// Makes a directory writable for as long as it lives and puts its mode back afterwards (agentd runs as root in the microVM and needs
/// nothing; a test runs as the owner of a directory an earlier attempt locked).
struct Writable {
    dir: PathBuf,
    mode: u32,
}

impl Writable {
    fn open(dir: &Path) -> std::io::Result<Writable> {
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o7777;
        if mode & 0o200 == 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o200))?;
        }
        Ok(Writable {
            dir: dir.to_path_buf(),
            mode,
        })
    }
}

impl Drop for Writable {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(self.mode));
    }
}

fn io(what: &str, e: std::io::Error) -> String {
    format!("skills: {what}: {e}")
}

/// Removes the folders under `dir` that agentd placed (they hold [`MARKER`]); `dir` is writable. Links are never followed and nothing
/// without the marker is touched.
fn retire_in(dir: &Path) -> Result<Vec<String>, String> {
    let mut gone = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| io("read the skills directory", e))? {
        let entry = entry.map_err(|e| io("read the skills directory", e))?;
        let path = entry.path();
        if !placed_by_agentd(&path) {
            continue;
        }
        unlock_tree(&path).map_err(|e| io("unlock an earlier attempt's skill", e))?;
        std::fs::remove_dir_all(&path).map_err(|e| io("remove an earlier attempt's skill", e))?;
        gone.push(entry.file_name().to_string_lossy().into_owned());
    }
    gone.sort();
    Ok(gone)
}

/// Removes the skill folders an earlier attempt of this run placed (the ones that hold [`MARKER`]) and nothing else; returns their names.
/// A directory that is not there, or is a link, holds nothing agentd placed. The mode of `dir` is what it was.
pub fn retire_placed(dir: &Path) -> Result<Vec<String>, String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io("read the skills directory", e)),
    }
    let _writable = Writable::open(dir).map_err(|e| io("open the skills directory", e))?;
    retire_in(dir)
}

/// The text of the marker of a folder: who placed it and the digest of the skill.
fn marker_text(skill: &Skill) -> String {
    format!(
        "{{\"placedBy\":\"agentd\",\"skill\":\"{}\",\"sha256\":\"{}\"}}\n",
        skill.name,
        skill_sha256(skill)
    )
}

/// Writes one file as the engine will read it: exact bytes, created new, mode 0444, owned by root.
fn write_locked(path: &Path, bytes: &[u8], what: &str) -> Result<(), String> {
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(path)
        .map_err(|e| io(&format!("write {what}"), e))?;
    out.write_all(bytes)
        .map_err(|e| io(&format!("write {what}"), e))?;
    drop(out);
    // The creation mode is cut by the umask; the final mode is set outright.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
        .map_err(|e| io(&format!("lock {what}"), e))?;
    crate::engine::direct::own_by_root(path).map_err(|e| io(&format!("own {what}"), e))
}

/// Writes `skills` under `dir` and locks what it wrote. With no skills nothing is created. The folders an earlier attempt placed are
/// replaced ([`retire_placed`]); anything else under `dir` stays as it is, its modes and the mode of `dir` included (a `dir` agentd creates is
/// locked, root-owned, 0555). A folder of a requested name that agentd did not place is refused before the first file is written. A link
/// where `dir` belongs is refused and never followed. Nothing is written unless every skill passed [`check_before_writing`].
pub fn place_skills(dir: &Path, skills: &[Skill]) -> Result<Vec<PlacedSkill>, String> {
    if skills.is_empty() {
        return Ok(Vec::new());
    }
    let docs = check_before_writing(skills)?;
    crate::engine::direct::refuse_symlink(dir).map_err(|_| {
        format!(
            "skills: the skills directory {} is a symbolic link",
            dir.display()
        )
    })?;
    let existed = dir.exists();
    if existed && !dir.is_dir() {
        return Err(format!(
            "skills: {} exists and is not a directory",
            dir.display()
        ));
    }
    // The root of the skills directory is writable while agentd works in it; an existing one gets its mode back, a new one is locked.
    let writable = if existed {
        let w = Writable::open(dir).map_err(|e| io("open the skills directory", e))?;
        retire_in(dir)?;
        Some(w)
    } else {
        std::fs::create_dir_all(dir).map_err(|e| io("create the directory", e))?;
        None
    };
    // A clash is found before the first file is written.
    for s in skills {
        if std::fs::symlink_metadata(dir.join(&s.name)).is_ok() {
            return Err(problem(&s.name, None, SkillProblem::Shadowed));
        }
    }
    let mut placed = Vec::with_capacity(skills.len());
    for (s, doc) in skills.iter().zip(docs) {
        let skill_dir = dir.join(&s.name);
        std::fs::create_dir(&skill_dir).map_err(|e| io(&format!("create {}", s.name), e))?;
        // The marker comes first: a folder that was begun is agentd's even if this attempt dies before the rest.
        write_locked(&skill_dir.join(MARKER), marker_text(s).as_bytes(), MARKER)?;
        let mut files = Vec::with_capacity(s.files.len());
        for f in &s.files {
            let path = skill_dir.join(&f.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io(&format!("create {}", f.path), e))?;
            }
            write_locked(&path, f.content.as_bytes(), &f.path)?;
            files.push(PlacedFile {
                path: f.path.clone(),
                sha256: f.sha256.clone(),
                bytes: f.size,
            });
        }
        placed.push(PlacedSkill {
            name: s.name.clone(),
            dir: skill_dir.clone(),
            skill_md: skill_dir.join(SKILL_ENTRY),
            body: doc.body,
            digest: skill_sha256(s),
            files,
        });
    }
    // Lock the directories agentd wrote, from the deepest up.
    let mut dirs = Vec::new();
    for p in &placed {
        dirs_below(&p.dir, &mut dirs).map_err(|e| io("list the directories", e))?;
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in &dirs {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555))
            .map_err(|e| io("lock a directory", e))?;
        crate::engine::direct::own_by_root(d).map_err(|e| io("own a directory", e))?;
    }
    match writable {
        // The directory was there: it keeps its mode (the guard puts it back) and its owner.
        Some(_) => {}
        None => {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))
                .map_err(|e| io("lock the skills directory", e))?;
            crate::engine::direct::own_by_root(dir)
                .map_err(|e| io("own the skills directory", e))?;
        }
    }
    Ok(placed)
}
