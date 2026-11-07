//! The skills of a direct run.
//!
//! A skill is a folder of files the engine reads when the model needs it: `SKILL.md` (a `name` and a `description` in YAML frontmatter,
//! then instructions) and, optionally, scripts and reference files beside it. The user's rules are always in the prompt; a skill is loaded
//! on demand, so it keeps the always-on prompt small. `ham` reads the skills (the lifecycle pack's `.agents/skills`, or a folder the user
//! names), sends them as the `skills` member of `StartDirectRunRequest`, and agentd writes them to the engine's configuration directory
//! (`<config dir>/skills/<name>/...`, root-owned, read-only, outside the repository). The request is stored as `runs/<run>/input/request.json`,
//! so the invocation body does not change.
//!
//! One validator serves all three ends (`ham`, control-api, agentd): [`check_skills`] is pure and returns the first problem as a
//! [`SkillError`] whose message names the skill, the file and the rule, never the content. Checks that need a disk (a symlink, a path that
//! escapes the folder when written, a repository skill of the same name) belong to the code that reads or writes the disk; their problems use
//! the same [`SkillProblem`] words.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The most skills in one run. A count stays because `summary.json` lists every skill (name, hash, file count and size) and is held to 64 KiB, and
/// because the name and description of every skill are listed in the system prompt of every turn: 32 skills weigh at most about 6 KiB in the
/// summary (`tests/b776_input_limits.rs` builds the worst case together with the rules) and at most 32 KiB of descriptions in the prompt. The
/// number of files of a skill and the size of a file have no bound of their own.
pub const MAX_SKILLS: usize = 32;
/// The longest skill name, in bytes.
pub const MAX_SKILL_NAME_BYTES: usize = 64;
/// The longest file path inside a skill, in bytes, directories included: the longest name a file system takes (255 bytes on ext4 and APFS), so no
/// part of a path can pass it either.
pub const MAX_SKILL_PATH_BYTES: usize = 255;
/// The most characters in the description of a skill: it is listed in the system prompt of every turn.
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
/// The file every skill must hold, at the top of its folder.
pub const SKILL_ENTRY: &str = "SKILL.md";

/// One file of a [`Skill`]. `path` is relative to the skill's folder, `/`-separated, with no `.` or `..` part. `content` is UTF-8 text with no
/// NUL byte; `size` is its length in bytes and `sha256` is the lowercase hex SHA-256 of those bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub content: String,
}

impl SkillFile {
    /// A file with its `size` and `sha256` computed from `content`.
    pub fn new(path: &str, content: &str) -> SkillFile {
        SkillFile {
            path: path.to_string(),
            sha256: sha256_hex(content.as_bytes()),
            size: content.len() as u64,
            content: content.to_string(),
        }
    }
}

/// One skill of the `skills` member of `StartDirectRunRequest` (protocol 1.38.0). `name` is the folder name and the `name` in the frontmatter of
/// `SKILL.md`: lowercase letters, digits and single hyphens, 1 to [`MAX_SKILL_NAME_BYTES`] bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub files: Vec<SkillFile>,
}

/// A skill as `summary.json` lists it (protocol 1.38.0): what the session was given, never the content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectSkillRef {
    pub name: String,
    /// [`skill_sha256`] of the skill.
    pub sha256: String,
    /// The number of files.
    pub files: u32,
    /// The total size of the files, in bytes.
    pub bytes: u64,
}

/// The rule a skill breaks. The words are the same in `ham`, control-api and agentd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillProblem {
    /// More than [`MAX_SKILLS`] skills.
    TooManySkills { count: usize, max: usize },
    /// The name is not `[a-z0-9]+(-[a-z0-9]+)*` of 1 to [`MAX_SKILL_NAME_BYTES`] bytes.
    BadName,
    /// Two skills have the same name.
    DuplicateSkill,
    /// The skill has no file.
    NoFiles,
    /// The path is empty, absolute, has a `.` or `..` part, a part outside `[A-Za-z0-9._-]`, a part that starts with `.`, is longer than
    /// [`MAX_SKILL_PATH_BYTES`].
    BadPath,
    /// Two files of a skill have the same path (compared without case: the Mac folds case, the VM does not).
    DuplicatePath,
    /// The skill has no [`SKILL_ENTRY`] at its top.
    MissingEntry,
    /// The entry file is empty.
    EmptyEntry,
    /// Read from a disk: a file is longer than a whole request can be (`max` is [`crate::direct::MAX_REQUEST_BYTES`]). [`check_skills`] never
    /// returns it: the size of the request as a whole is judged by [`crate::direct::check_request`].
    TooLarge { bytes: usize, max: usize },
    /// `size` is not the length of `content`.
    SizeMismatch,
    /// `sha256` is not the SHA-256 of `content`.
    ShaMismatch,
    /// `content` holds a NUL byte.
    Nul,
    /// One path of the skill is the start of another path of it (`SKILL.md` and `SKILL.md/x`): a name cannot be a file and a folder.
    PathConflict,
    /// Read from disks: the files of the skills read together are longer than a whole request can be. Not returned by [`check_skills`] either.
    TooLargeTotal { bytes: usize, max: usize },
    /// `SKILL.md` is outside the documented subset (see [`skill_document`]); the words say which rule.
    BadEntry(&'static str),
    /// Read from a disk: the entry is a symbolic link, a directory where a file belongs, or another kind of entry.
    NotRegular,
    /// Read from a disk: the folder holds a skill of this name already (a repository skill of the same name, or a second source).
    Shadowed,
}

impl SkillProblem {
    /// A stable word for the problem (snake case), the same in every crate and in the shared test cases.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooManySkills { .. } => "too_many_skills",
            Self::BadName => "bad_name",
            Self::DuplicateSkill => "duplicate_skill",
            Self::NoFiles => "no_files",
            Self::BadPath => "bad_path",
            Self::DuplicatePath => "duplicate_path",
            Self::MissingEntry => "missing_entry",
            Self::EmptyEntry => "empty_entry",
            Self::TooLarge { .. } => "too_large",
            Self::SizeMismatch => "size_mismatch",
            Self::ShaMismatch => "sha_mismatch",
            Self::Nul => "nul",
            Self::PathConflict => "path_conflict",
            Self::TooLargeTotal { .. } => "too_large_total",
            Self::BadEntry(_) => "bad_entry",
            Self::NotRegular => "not_regular",
            Self::Shadowed => "shadowed",
        }
    }
}

impl std::fmt::Display for SkillProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManySkills { count, max } => {
                write!(f, "{count} skills; the limit is {max}")
            }
            Self::BadName => write!(
                f,
                "has a name that is not allowed: use 1-{MAX_SKILL_NAME_BYTES} lowercase letters, digits and single hyphens, not starting or ending with a hyphen"
            ),
            Self::DuplicateSkill => write!(f, "appears twice"),
            Self::NoFiles => write!(f, "has no files"),
            Self::BadPath => write!(
                f,
                "has a file path that is not allowed: a relative path of at most {MAX_SKILL_PATH_BYTES} bytes, each part letters, digits, '.', '_' or '-' and not starting with '.'"
            ),
            Self::DuplicatePath => write!(
                f,
                "has two files with one path (paths are compared without case)"
            ),
            Self::MissingEntry => write!(f, "has no {SKILL_ENTRY} at the top of its folder"),
            Self::EmptyEntry => write!(f, "has an empty {SKILL_ENTRY}"),
            Self::TooLarge { bytes, .. } => write!(
                f,
                "has a file of {bytes} bytes; a request carries at most {}, so this file can never be sent",
                crate::direct_request::bound_words()
            ),
            Self::SizeMismatch => write!(f, "has a size that is not its content's length"),
            Self::ShaMismatch => write!(f, "has a sha256 that is not its content's"),
            Self::Nul => write!(f, "has a file with a NUL byte"),
            Self::PathConflict => write!(
                f,
                "has a path that is both a file and a folder (for example SKILL.md and SKILL.md/x)"
            ),
            Self::TooLargeTotal { bytes, .. } => write!(
                f,
                "the files of all skills together hold {bytes} bytes; a request carries at most {}",
                crate::direct_request::bound_words()
            ),
            Self::BadEntry(reason) => write!(f, "{reason}"),
            Self::NotRegular => write!(
                f,
                "holds something that is not a regular file (a symbolic link or a directory where a file belongs)"
            ),
            Self::Shadowed => write!(f, "has the name of a skill that is already there"),
        }
    }
}

/// The first rule a set of skills breaks, with the skill and the file it names (neither holds content; both are cut and made printable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillError {
    pub skill: Option<String>,
    pub file: Option<String>,
    pub problem: SkillProblem,
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.skill, &self.file) {
            (Some(s), Some(file)) => write!(f, "skill {s:?} file {file:?} {}", self.problem),
            (Some(s), None) => write!(f, "skill {s:?} {}", self.problem),
            _ => write!(f, "skills: {}", self.problem),
        }
    }
}

impl std::error::Error for SkillError {}

impl SkillError {
    /// An error naming a skill and a file; both are cut and made printable (a hostile name must not reach a terminal or a log raw).
    pub fn new(skill: Option<&str>, file: Option<&str>, problem: SkillProblem) -> SkillError {
        SkillError {
            skill: skill.map(printable),
            file: file.map(printable),
            problem,
        }
    }
}

/// `text` cut to 40 characters, with every character that is not printable ASCII replaced by `?`.
fn printable(text: &str) -> String {
    let mut out: String = text
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    if text.chars().count() > 40 {
        out.push_str("...");
    }
    out
}

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `name` is `[a-z0-9]+(-[a-z0-9]+)*` of 1 to [`MAX_SKILL_NAME_BYTES`] bytes.
pub fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_BYTES
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Whether `path` is a relative, `/`-separated path of at most [`MAX_SKILL_PATH_BYTES`] bytes, each part letters, digits, `.`, `_` or `-` and
/// not starting with `.`. There is no limit on the depth.
pub fn valid_skill_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_SKILL_PATH_BYTES
        && path.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

// ---- the SKILL.md reader -------------------------------------------------------------------------------------------------------------

const NO_FRONTMATTER: &str = "has no frontmatter: it must start with a line of three hyphens";
const UNCLOSED: &str = "has no frontmatter: the line of three hyphens that ends it is missing";
const NAME_TWICE: &str = "has the key name or description twice in its frontmatter";
const NAME_BLOCK: &str =
    "has a frontmatter name written as a block; the name is one line of plain text or in quotes";
const PLAIN_COLON: &str = "has a plain name or description that contains a colon and a space, or ends with a colon: put the text in quotes";
const UNSUPPORTED: &str = "has a name or description that uses YAML beyond plain text, quotes without escapes or a block (no lists, anchors, tags or escapes): put the text in plain quotes";
const NAME_MISMATCH: &str = "has a frontmatter name that is not the folder name";
const NO_DESCRIPTION: &str = "has no description in its frontmatter";
const NO_BODY: &str = "has no instructions after its frontmatter (the body is empty)";
const DESCRIPTION_LONG: &str = "has a description of more than 1024 characters";

/// What the engine lists from a `SKILL.md`: the skill's name (the folder's), its description and the instructions after the frontmatter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Reads a `SKILL.md` in the documented subset; `folder` is the skill's name. The pinned engine reads the frontmatter with a YAML parser and
/// lists a skill under its frontmatter `name`, only when the body is not empty (measured 2026-10-16). `check_skills` calls this function at the
/// request and agentd calls it before it writes, so the two ends accept exactly the same texts. The subset:
///
/// - the text starts with a line of three hyphens (LF or CRLF), and the frontmatter ends at the next line that is only three hyphens;
/// - only `name` and `description` are read, each at most once; every other key, comment line, blank line and indented line below another key
///   is passed over;
/// - a value is plain text (a ` #` starts a comment; it may not hold a colon followed by a space, end with a colon or start with a YAML
///   indicator such as `[`, `{`, `&`, `*`, `!`, `|`, `>` or `%`), or text in single quotes (`''` is one quote) or double quotes (no backslash),
///   with an optional comment after the closing quote;
/// - plain text may continue on the following indented lines, which join with single spaces; `description:` with its text on the next
///   indented line is the same thing;
/// - `description` may also be a block: `>` or `|`, optionally with `-` or `+`, followed by indented lines (read as one line of text);
/// - `name` must be the folder name, `description` must have text (at most [`MAX_SKILL_DESCRIPTION_CHARS`] characters) and the body (the
///   text after the closing line, trimmed at both ends, line endings kept) must not be empty.
///
/// Anything else in `name` or `description` is refused with words that say which rule, never the text.
pub fn skill_document(text: &str, folder: &str) -> Result<SkillDoc, &'static str> {
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))
        .ok_or(NO_FRONTMATTER)?;
    let mut front: Vec<&str> = Vec::new();
    let mut consumed = 0usize;
    let mut closed = false;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare == "---" {
            closed = true;
            break;
        }
        front.push(bare);
    }
    if !closed {
        return Err(UNCLOSED);
    }
    let body = rest[consumed..].trim().to_string();
    let (mut name, mut description): (Option<String>, Option<String>) = (None, None);
    let mut i = 0;
    while i < front.len() {
        let line = front[i];
        i += 1;
        if line.trim().is_empty()
            || line.trim_start().starts_with('#')
            || line.starts_with([' ', '\t'])
        {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // The lines below the key that are indented (or blank, or comments) belong to its value.
        let mut more: Vec<&str> = Vec::new();
        while i < front.len() {
            let next = front[i];
            if next.starts_with([' ', '\t'])
                || next.trim().is_empty()
                || next.trim_start().starts_with('#')
            {
                more.push(next);
                i += 1;
            } else {
                break;
            }
        }
        let which = match key.trim() {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        if which.is_some() {
            return Err(NAME_TWICE);
        }
        let is_name = key.trim() == "name";
        *which = Some(read_value(value, &more, is_name)?);
    }
    match name.as_deref() {
        Some(n) if n == folder => {}
        _ => return Err(NAME_MISMATCH),
    }
    let description = description.unwrap_or_default();
    if description.trim().is_empty() {
        return Err(NO_DESCRIPTION);
    }
    if description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
        return Err(DESCRIPTION_LONG);
    }
    if body.is_empty() {
        return Err(NO_BODY);
    }
    Ok(SkillDoc {
        name: folder.to_string(),
        description,
        body,
    })
}

/// The text of one `name` or `description` value: `value` is what follows the colon on the key's line, `more` the lines below it that
/// belong to it.
fn read_value(value: &str, more: &[&str], is_name: bool) -> Result<String, &'static str> {
    let v = value.trim();
    let lines_of = |more: &[&str]| -> Vec<String> {
        more.iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect()
    };
    // A block scalar: the header, then the indented lines.
    if v.starts_with(['>', '|']) {
        let header = strip_comment(v).trim();
        if !matches!(header, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            return Err(UNSUPPORTED);
        }
        if is_name {
            return Err(NAME_BLOCK);
        }
        return Ok(lines_of(more).join(" "));
    }
    // Quoted on one line, optionally followed by a comment.
    if let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') {
        if !more
            .iter()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        {
            return Err(UNSUPPORTED);
        }
        return read_quoted(&v[1..], q);
    }
    // Plain text, possibly over more lines.
    let first = strip_comment(v).trim();
    let mut parts: Vec<String> = Vec::new();
    if !first.is_empty() {
        parts.push(first.to_string());
    }
    for l in lines_of(more) {
        parts.push(strip_comment(&l).trim().to_string());
    }
    for part in &parts {
        if part.contains(": ") || part.ends_with(':') {
            return Err(PLAIN_COLON);
        }
    }
    if let Some(first) = parts.first() {
        let mut chars = first.chars();
        let c = chars.next().unwrap_or(' ');
        let after = chars.next();
        let indicator = matches!(
            c,
            '[' | ']' | '{' | '}' | '#' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`'
        ) || (matches!(c, '-' | '?' | ':') && after.is_none_or(|a| a == ' '));
        if indicator {
            return Err(UNSUPPORTED);
        }
    }
    Ok(parts.join(" "))
}

/// `text` without a comment: a ` #` (a hash after a space) and everything after it.
fn strip_comment(text: &str) -> &str {
    match text.find(" #") {
        Some(i) => &text[..i],
        None => text,
    }
}

/// The inside of a quoted value; `after_open` is the text after the opening quote `q`. A double-quoted value may not hold a backslash; a
/// single-quoted one writes a quote as two.
fn read_quoted(after_open: &str, q: char) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = after_open.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' && q == '"' {
            return Err(UNSUPPORTED);
        }
        if c == q {
            if q == '\'' && matches!(chars.peek(), Some((_, '\''))) {
                chars.next();
                out.push('\'');
                continue;
            }
            // The closing quote: only space and a comment may follow.
            let tail = after_open[i + c.len_utf8()..].trim();
            if tail.is_empty() || tail.starts_with('#') {
                return Ok(out);
            }
            return Err(UNSUPPORTED);
        }
        out.push(c);
    }
    Err(UNSUPPORTED)
}

/// Checks the `skills` member of a start request: the count, names, paths, hashes, the entry file and its frontmatter. Pure; the first
/// problem wins, in this order: the number of skills; then for each skill its name, a repeated name; then for each file its path, a repeated
/// path (compared without case), a NUL byte, its `size`, its `sha256` (64 lowercase hex digits: another spelling of the hash is refused); then
/// a path that is also the start of another path of the skill; then the entry file (present, not empty, readable by [`skill_document`] under
/// the skill's name). No size is judged here: the request as a whole is held to [`crate::direct::MAX_REQUEST_BYTES`] by
/// [`crate::direct::check_request`].
pub fn check_skills(skills: &[Skill]) -> Result<(), SkillError> {
    if skills.len() > MAX_SKILLS {
        return Err(SkillError::new(
            None,
            None,
            SkillProblem::TooManySkills {
                count: skills.len(),
                max: MAX_SKILLS,
            },
        ));
    }
    let mut names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for skill in skills {
        let fail = |file: Option<&str>, problem| SkillError::new(Some(&skill.name), file, problem);
        if !valid_skill_name(&skill.name) {
            return Err(fail(None, SkillProblem::BadName));
        }
        if !names.insert(skill.name.as_str()) {
            return Err(fail(None, SkillProblem::DuplicateSkill));
        }
        if skill.files.is_empty() {
            return Err(fail(None, SkillProblem::NoFiles));
        }
        let mut paths: Vec<String> = Vec::with_capacity(skill.files.len());
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(skill.files.len());
        for f in &skill.files {
            let at = Some(f.path.as_str());
            if !valid_skill_path(&f.path) {
                return Err(fail(at, SkillProblem::BadPath));
            }
            let folded = f.path.to_ascii_lowercase();
            if !seen.insert(folded.clone()) {
                return Err(fail(at, SkillProblem::DuplicatePath));
            }
            paths.push(folded);
            if f.content.contains('\0') {
                return Err(fail(at, SkillProblem::Nul));
            }
            if f.size != f.content.len() as u64 {
                return Err(fail(at, SkillProblem::SizeMismatch));
            }
            // 64 lowercase hex digits and nothing else: agentd compares the hash exactly, so a request that passed here with another
            // spelling of the same hash would fail only when the run starts.
            if f.sha256 != sha256_hex(f.content.as_bytes()) {
                return Err(fail(at, SkillProblem::ShaMismatch));
            }
        }
        // A path may not be the start of another path of the skill: a name cannot be a file and a folder on a disk. Each path is asked for its
        // parents in the set of paths (a skill has no file count to keep this short), and the first shorter path in file order wins.
        let index: std::collections::HashMap<&str, usize> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();
        let mut conflict: Option<(usize, usize)> = None;
        for (longer, path) in paths.iter().enumerate() {
            for (cut, _) in path.match_indices('/') {
                if let Some(&shorter) = index.get(&path[..cut])
                    && conflict.is_none_or(|c| (shorter, longer) < c)
                {
                    conflict = Some((shorter, longer));
                }
            }
        }
        if let Some((_, longer)) = conflict {
            return Err(fail(
                Some(skill.files[longer].path.as_str()),
                SkillProblem::PathConflict,
            ));
        }
        match skill.files.iter().find(|f| f.path == SKILL_ENTRY) {
            None => return Err(fail(None, SkillProblem::MissingEntry)),
            Some(entry) if entry.content.is_empty() => {
                return Err(fail(Some(SKILL_ENTRY), SkillProblem::EmptyEntry));
            }
            Some(entry) => {
                if let Err(reason) = skill_document(&entry.content, &skill.name) {
                    return Err(fail(Some(SKILL_ENTRY), SkillProblem::BadEntry(reason)));
                }
            }
        }
    }
    Ok(())
}

/// The digest of one skill: the lowercase hex SHA-256 of the lines `<path>\t<sha256>\n` of its files in path order, each hash taken from the
/// file's content. Two skills with the same files have the same digest whatever the order they were sent in or the name they carry.
pub fn skill_sha256(skill: &Skill) -> String {
    let mut lines: Vec<(&str, String)> = skill
        .files
        .iter()
        .map(|f| (f.path.as_str(), sha256_hex(f.content.as_bytes())))
        .collect();
    lines.sort();
    let mut text = String::new();
    for (path, sha) in lines {
        text.push_str(path);
        text.push('\t');
        text.push_str(&sha);
        text.push('\n');
    }
    sha256_hex(text.as_bytes())
}
