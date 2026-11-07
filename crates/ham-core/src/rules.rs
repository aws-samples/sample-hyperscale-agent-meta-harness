//! The user's rules. `ham` sends the files of `~/.config/ham/rules/` with each direct run
//! and control-api stores them; agentd writes them to Cline's global rules directory. One validator serves both ends, so
//! `ham` refuses a bad file before a run starts and the Lambda refuses what a stale or hand-written client sends.
//!
//! A rules directory is flat: regular `.md` files with ASCII names. Rules steer Cline's behaviour and nothing here judges
//! their content; the checks only keep out what would silently break the loader or the run:
//!
//! - an empty file, or a NUL byte or a byte order mark (a rule file has no size of its own: the request as a whole is held to what the platform
//!   takes, `harness_protocol::direct::MAX_REQUEST_BYTES`);
//! - `---` at the top without a closed, valid YAML mapping behind it (Cline would take it for frontmatter), and YAML
//!   anchors or aliases in it (a small file can expand into gigabytes);
//! - the name `AGENTS.md` in any case (Cline loads such a file from a rules directory as a repository instruction file);
//! - names that are not flat ASCII (`/`, `..`, spaces, unicode: the Mac and the VM normalise unicode differently);
//! - two names that differ only in case (APFS folds them, ext4 does not), more than 100 files (the end-of-run summary lists every file), a
//!   `sha256` that is not the content's.
//!
//! `continue.md` holds the nudge text and, in optional frontmatter, `max_nudges`.
//!
//! `review.md` holds the review prompt: the text the harness sends once, in the same session, after the engine reports done with a changed
//! file, and in optional frontmatter `guard_minutes` (1 to 480; the least time a run must have left for the round to start; without it the stage's own
//! 20 minutes). `{{base}}` and `{{facts}}` in the text are filled in by the harness. The default file is the protocol's default prompt.

use crate::config::config_path;
use harness_protocol::api::v1::{ContinueSettings, RuleFile};
use harness_protocol::direct::{
    DEFAULT_REVIEW_PROMPT, REVIEW_GUARD_S, ReviewOptions, bound_words, check_review,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The most rule files in one run, and the longest rule file name in bytes (`.md` included): one definition for `ham`, control-api and agentd, in
/// the protocol crate. A rule file, `continue.md` and `review.md` have no size of their own: the whole request is held to what the platform takes.
pub use harness_protocol::direct::{MAX_RULE_FILES, MAX_RULE_NAME_BYTES as MAX_NAME_BYTES};
/// The default count of nudges (0: off, so Cline is left as it is). The count has no ceiling: agentd stops nudging when the work tree stays
/// unchanged and when the run's time is used, and the user sets the number.
pub const DEFAULT_MAX_NUDGES: u32 = 0;
/// The file `ham rules init` writes.
pub const DEFAULT_RULES_NAME: &str = "00-my-rules.md";
pub const DEFAULT_RULES: &str = include_str!("../assets/default-rules.md");
/// The nudge file `ham rules init` writes, and the name `RuleError`s report for it.
pub const CONTINUE_NAME: &str = "continue.md";
pub const DEFAULT_CONTINUE: &str = include_str!("../assets/default-continue.md");
/// The review prompt file `ham rules init` writes, and the name `RuleError`s report for it.
pub const REVIEW_NAME: &str = "review.md";
/// The frontmatter key of `review.md`.
pub const REVIEW_GUARD_KEY: &str = "guard_minutes";
/// The review file `ham rules init` writes: a comment that names the one key, then the default prompt of the protocol (the one text agentd also
/// uses when a request carries none). The key is commented out, so the stage's own guard applies until the user sets one.
pub fn default_review_file() -> String {
    format!(
        "---\n# guard_minutes: 20   (1 to 480) the least time a run must have left for the review round to start\n---\n{DEFAULT_REVIEW_PROMPT}"
    )
}

/// What is wrong with one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    Empty,
    TooLarge {
        bytes: usize,
        max: usize,
    },
    BadName,
    ReservedName,
    Nul,
    Bom,
    FrontmatterUnclosed,
    FrontmatterYaml(String),
    FrontmatterNotMapping,
    FrontmatterAnchor,
    ShaMismatch,
    Duplicate(String),
    TooMany {
        count: usize,
        max: usize,
    },
    /// `continue.md`: the count is not a whole number.
    NudgeCount(String),
    /// `continue.md`: a frontmatter key other than `max_nudges`.
    UnknownKey(String),
    /// `review.md`: a frontmatter key other than `guard_minutes`.
    UnknownReviewKey(String),
    /// `review.md`: `guard_minutes` is not a whole number from 1 to 480.
    ReviewGuard(String),
    /// Read from a disk: a directory or another kind of entry where a regular file belongs.
    NotRegular(String),
    NotUtf8,
    /// Read from a disk: the operating system's reason.
    Unreadable(String),
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Empty => write!(f, "is empty; write the rule or delete the file"),
            Problem::TooLarge { bytes, .. } => write!(
                f,
                "is {bytes} bytes; a request carries at most {}, so this file can never be sent",
                bound_words()
            ),
            Problem::BadName => write!(
                f,
                "has a name that is not allowed: use 1-{MAX_NAME_BYTES} letters, digits, '.', '_' or '-' (ASCII, no spaces or \
                 slashes, not starting with '.' or '-'), ending in .md"
            ),
            Problem::ReservedName => write!(
                f,
                "cannot be called AGENTS.md: Cline would load it as a repository instruction file; rename it"
            ),
            Problem::Nul => write!(f, "contains a NUL byte"),
            Problem::Bom => write!(
                f,
                "starts with a byte order mark; save it as UTF-8 without one"
            ),
            Problem::FrontmatterUnclosed => write!(
                f,
                "starts with --- but the frontmatter is never closed; start with text, or close it with a second --- line"
            ),
            Problem::FrontmatterYaml(e) => write!(f, "has frontmatter that is not valid YAML: {e}"),
            Problem::FrontmatterNotMapping => write!(
                f,
                "has frontmatter between --- lines that is not a key: value mapping; if the first --- is a divider, remove it"
            ),
            Problem::FrontmatterAnchor => {
                write!(
                    f,
                    "uses YAML anchors or aliases (& or *) in its frontmatter, which are not allowed"
                )
            }
            Problem::ShaMismatch => write!(f, "has a sha256 that does not match its content"),
            Problem::Duplicate(n) => write!(
                f,
                "is a duplicate of {n:?} (names are compared without case, because the Mac folds case and the VM does not)"
            ),
            Problem::TooMany { count, max } => {
                write!(
                    f,
                    "makes {count} rule files; a run takes at most {max} (the end-of-run summary lists every file)"
                )
            }
            Problem::NudgeCount(m) => write!(f, "{m}"),
            Problem::UnknownKey(k) => write!(
                f,
                "has the unknown frontmatter key {k:?}; the only key is max_nudges"
            ),
            Problem::UnknownReviewKey(k) => write!(
                f,
                "has the unknown frontmatter key {k:?}; the only key is {REVIEW_GUARD_KEY}"
            ),
            Problem::ReviewGuard(m) => write!(f, "{m}"),
            Problem::NotRegular(what) => write!(
                f,
                "is a {what}; a rules directory holds only regular .md files"
            ),
            Problem::NotUtf8 => write!(f, "is not UTF-8 text"),
            Problem::Unreadable(e) => write!(f, "cannot be read: {e}"),
        }
    }
}

/// One file's problem, with the file's name (`path` of the wire form, or `continue.md`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("rule {name:?} {problem}")]
pub struct RuleError {
    pub name: String,
    pub problem: Problem,
}

impl RuleError {
    pub fn new(name: &str, problem: Problem) -> RuleError {
        RuleError {
            name: name.to_string(),
            problem,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of `text` as lowercase hex.
pub fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// The wire form of a rule file, its sha256 computed.
pub fn rule_file(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.to_string(),
        sha256: sha256_hex(content),
        content: content.to_string(),
    }
}

/// Whether `name` is an allowed rule file name.
pub fn validate_rule_name(name: &str) -> Result<(), Problem> {
    if name.eq_ignore_ascii_case("agents.md") {
        return Err(Problem::ReservedName);
    }
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    let first_ok = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    let stem_ok = name.len() > 3 && name.ends_with(".md");
    if name.len() > MAX_NAME_BYTES || !ok_chars || !first_ok || !stem_ok {
        return Err(Problem::BadName);
    }
    Ok(())
}

/// The YAML text and the body of a document that opens with a `---` line, or the whole text as the body.
fn split_frontmatter(text: &str) -> Result<(Option<&str>, &str), Problem> {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok((None, text));
    };
    if first.trim_end() != "---" {
        return Ok((None, text));
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end() == "---" {
            let yaml = &text[first.len()..offset];
            return Ok((Some(yaml), &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err(Problem::FrontmatterUnclosed)
}

/// True when the YAML has a node that starts with `&name` (an anchor) or `*name` (an alias): after the start of a line,
/// whitespace, `[`, `{` or `,`. A quoted glob such as `'*.rs'` or a path such as `src/**/*.rs` does not match.
fn has_anchor_or_alias(yaml: &str) -> bool {
    let b = yaml.as_bytes();
    (0..b.len().saturating_sub(1)).any(|i| {
        matches!(b[i], b'&' | b'*')
            && (b[i + 1].is_ascii_alphanumeric() || matches!(b[i + 1], b'_' | b'-'))
            && (i == 0 || matches!(b[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'[' | b'{' | b','))
    })
}

fn frontmatter(yaml: &str) -> Result<serde_yaml_ng::Value, Problem> {
    if yaml.trim().is_empty() {
        return Ok(serde_yaml_ng::Value::Null);
    }
    if has_anchor_or_alias(yaml) {
        return Err(Problem::FrontmatterAnchor);
    }
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml)
        .map_err(|e| Problem::FrontmatterYaml(e.to_string()))
}

/// The checks every file shares: NUL, byte order mark and, for a document that opens with `---`, valid frontmatter.
/// Returns the body after the frontmatter and the frontmatter itself.
fn read_document(text: &str) -> Result<(String, Option<serde_yaml_ng::Value>), Problem> {
    if text.contains('\0') {
        return Err(Problem::Nul);
    }
    if text.starts_with('\u{feff}') {
        return Err(Problem::Bom);
    }
    let (yaml, body) = split_frontmatter(text)?;
    let front = match yaml {
        None => None,
        Some(y) => match frontmatter(y)? {
            serde_yaml_ng::Value::Null => Some(serde_yaml_ng::Value::Null),
            m @ serde_yaml_ng::Value::Mapping(_) => Some(m),
            _ => return Err(Problem::FrontmatterNotMapping),
        },
    };
    if body.trim().is_empty() {
        return Err(Problem::Empty);
    }
    Ok((body.to_string(), front))
}

/// One rule file: its name and its content.
pub fn validate_rule(name: &str, content: &str) -> Result<(), RuleError> {
    validate_rule_name(name)
        .and_then(|()| read_document(content).map(|_| ()))
        .map_err(|p| RuleError::new(name, p))
}

/// A whole upload: at most [`MAX_RULE_FILES`] files, each valid, no two names equal ignoring case, each `sha256` the
/// content's (case ignored). The first problem found, in file order.
pub fn validate_rule_files(files: &[RuleFile]) -> Result<(), RuleError> {
    if files.len() > MAX_RULE_FILES {
        return Err(RuleError::new(
            "rules",
            Problem::TooMany {
                count: files.len(),
                max: MAX_RULE_FILES,
            },
        ));
    }
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(files.len());
    for f in files {
        validate_rule(&f.path, &f.content)?;
        if !f.sha256.eq_ignore_ascii_case(&sha256_hex(&f.content)) {
            return Err(RuleError::new(&f.path, Problem::ShaMismatch));
        }
        if !seen.insert(f.path.to_ascii_lowercase()) {
            return Err(RuleError::new(&f.path, Problem::Duplicate(f.path.clone())));
        }
    }
    Ok(())
}

fn nudge_error(m: impl Into<String>) -> RuleError {
    RuleError::new(CONTINUE_NAME, Problem::NudgeCount(m.into()))
}

fn count_from(front: &serde_yaml_ng::Value) -> Result<u32, RuleError> {
    let serde_yaml_ng::Value::Mapping(map) = front else {
        return Ok(DEFAULT_MAX_NUDGES);
    };
    let mut count = DEFAULT_MAX_NUDGES;
    for (k, v) in map {
        let key = k.as_str().unwrap_or("?");
        if key != "max_nudges" {
            return Err(RuleError::new(
                CONTINUE_NAME,
                Problem::UnknownKey(key.to_string()),
            ));
        }
        count = match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) => n,
            None => {
                return Err(nudge_error(format!(
                    "max_nudges must be a whole number (0 turns the nudge off), got {}",
                    serde_yaml_ng::to_string(v).unwrap_or_default().trim()
                )));
            }
        };
    }
    Ok(count)
}

/// `continue.md`: optional frontmatter with `max_nudges`, then the nudge text.
pub fn parse_continue(text: &str) -> Result<ContinueSettings, RuleError> {
    let wrap = |p: Problem| RuleError::new(CONTINUE_NAME, p);
    let (body, front) = read_document(text).map_err(wrap)?;
    let max_nudges = match &front {
        Some(f) => count_from(f)?,
        None => DEFAULT_MAX_NUDGES,
    };
    let settings = ContinueSettings {
        sha256: sha256_hex(&body),
        text: body,
        max_nudges,
    };
    validate_continue(&settings)?;
    Ok(settings)
}

/// A `ContinueSettings` as control-api receives it: text present and without a NUL byte, the `sha256` the text's. The text has no size of its
/// own and the count has no ceiling.
pub fn validate_continue(c: &ContinueSettings) -> Result<(), RuleError> {
    let wrap = |p: Problem| RuleError::new(CONTINUE_NAME, p);
    if c.text.trim().is_empty() {
        return Err(wrap(Problem::Empty));
    }
    if c.text.contains('\0') {
        return Err(wrap(Problem::Nul));
    }
    if !c.sha256.eq_ignore_ascii_case(&sha256_hex(&c.text)) {
        return Err(wrap(Problem::ShaMismatch));
    }
    Ok(())
}

fn review_error(m: impl Into<String>) -> RuleError {
    RuleError::new(REVIEW_NAME, Problem::ReviewGuard(m.into()))
}

fn guard_from(front: &serde_yaml_ng::Value) -> Result<Option<u32>, RuleError> {
    let serde_yaml_ng::Value::Mapping(map) = front else {
        return Ok(None);
    };
    let mut guard = None;
    let (min, max) = (REVIEW_GUARD_S.min / 60, REVIEW_GUARD_S.max / 60);
    for (k, v) in map {
        let key = k.as_str().unwrap_or("?");
        if key != REVIEW_GUARD_KEY {
            return Err(RuleError::new(
                REVIEW_NAME,
                Problem::UnknownReviewKey(key.to_string()),
            ));
        }
        guard = Some(match v.as_u64() {
            Some(n) if (u64::from(min)..=u64::from(max)).contains(&n) => n as u32 * 60,
            Some(n) => {
                return Err(review_error(format!(
                    "{REVIEW_GUARD_KEY} is {n}; it must be a whole number from {min} to {max}"
                )));
            }
            None => {
                return Err(review_error(format!(
                    "{REVIEW_GUARD_KEY} must be a whole number from {min} to {max}, got {}",
                    serde_yaml_ng::to_string(v).unwrap_or_default().trim()
                )));
            }
        });
    }
    Ok(guard)
}

/// `review.md`: optional frontmatter with `guard_minutes`, then the review prompt. The member of the request it makes has the round on (`auto`),
/// the body as typed (`{{base}}` and `{{facts}}` are the harness's to fill in) and the guard in seconds when the file names one.
pub fn parse_review(text: &str) -> Result<ReviewOptions, RuleError> {
    let wrap = |p: Problem| RuleError::new(REVIEW_NAME, p);
    let (body, front) = read_document(text).map_err(wrap)?;
    let guard_s = match &front {
        Some(f) => guard_from(f)?,
        None => None,
    };
    let options = ReviewOptions::with_text(&body, guard_s);
    validate_review(&options)?;
    Ok(options)
}

/// A `review` member as control-api receives it, with the words of this file's checks: the one check of the protocol (`check_review`) names the
/// member; here the same problems are named after the file.
pub fn validate_review(r: &ReviewOptions) -> Result<(), RuleError> {
    use harness_protocol::direct::ReviewProblem as P;
    let wrap = |p: Problem| RuleError::new(REVIEW_NAME, p);
    check_review(r).map_err(|e| match e {
        P::TextEmpty => wrap(Problem::Empty),
        P::TextNul => wrap(Problem::Nul),
        P::Sha => wrap(Problem::ShaMismatch),
        P::Guard => review_error(format!(
            "{REVIEW_GUARD_KEY} must be a whole number from {} to {}",
            REVIEW_GUARD_S.min / 60,
            REVIEW_GUARD_S.max / 60
        )),
    })
}

/// `~/.config/ham/rules` (or under `XDG_CONFIG_HOME`), beside `config.toml`; `None` without `HOME`.
pub fn rules_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    config_path(env).map(|p| p.with_file_name("rules"))
}

/// `~/.config/ham/continue.md`, beside `config.toml`.
pub fn continue_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    config_path(env).map(|p| p.with_file_name(CONTINUE_NAME))
}

/// `~/.config/ham/review.md`, beside `config.toml`.
pub fn review_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    config_path(env).map(|p| p.with_file_name(REVIEW_NAME))
}
