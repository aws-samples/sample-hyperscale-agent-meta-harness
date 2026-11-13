//! The lifecycle pack as the client carries it.
//!
//! The pack is one contract (`AGENTS.md`) in three texts and seven skills (`.agents/skills/lifecycle-<step>/SKILL.md`, one with a script).
//! `ham` carries the bytes in the binary, so an install needs no network and no repository checkout: the contract is
//! [`CONTRACT`] (`assets/lifecycle-rules.md`, byte for byte `lifecycle/pack/AGENTS.md`: the `auto` text), with [`CONTRACT_FULL`] and
//! [`CONTRACT_QUICK`] beside it,
//! the other files are read from `lifecycle/pack/` at
//! build time. `tests/pack.rs` holds every embedded file to `lifecycle/pack.lock.json`, so a pack file that changes without the lock fails.
//!
//! Three ways the pack reaches a run (`lifecycle/AGENTS.md`, routes T0a, T0b, T1, T2 of the pack design):
//!
//! - T0a, [`contract_rule`] and [`contract_rule_for`]: the contract as the user rule [`CONTRACT_RULE`] (`10-lifecycle.md`), beside `00-my-rules.md`.
//! - T0b, [`skills_as_rules`]: each skill body, frontmatter removed, as an always-on rule `2n-lifecycle-<step>.md` (for measuring the pack
//!   before real skills; about 2,000 to 3,500 more tokens on every turn).
//! - T1, [`skills`]: the skills as the `skills` member of the start request, which agentd writes beside the rules.
//! - T2, [`files`]: the pack copied into a repository (`ham lifecycle init`).
//!
//! An install is brought up to date by `ham rules init`: a file in the user's own folders that equals an earlier text of the same pack
//! path (`lifecycle/pack.history.json`, [`History`]) was never edited and is replaced; every other file is the user's and is kept. A
//! repository (`ham lifecycle init`) is never changed this way. The user's default rules file (`00-my-rules.md`) is brought up to date the same
//! way from its own list of earlier texts (`assets/default-rules.history.json`, [`refresh_default_rules`]).

use crate::config::LifecycleFlow;
use crate::rules::{self, RuleError, rule_file, validate_rule};
use harness_protocol::api::v1::RuleFile;
use harness_protocol::direct::{Skill, SkillFile};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path};
use std::sync::OnceLock;

/// The pack's name on the command line: `ham rules init --pack lifecycle`.
pub const NAME: &str = "lifecycle";
/// The rule file the contract becomes (`AGENTS.md` itself is never a rule name: Cline would load it as a repository instruction file).
pub const CONTRACT_RULE: &str = "10-lifecycle.md";
/// The contract, byte for byte `lifecycle/pack/AGENTS.md`: the `auto` text (the flow statement, the quick path and the eight steps).
pub const CONTRACT: &str = include_str!("../assets/lifecycle-rules.md");
/// The `full` text of the contract: the eight steps alone (`lifecycle/contract.py` renders it from `lifecycle/contract.src`).
pub const CONTRACT_FULL: &str = include_str!("../assets/lifecycle-rules-full.md");
/// The `quick` text of the contract: the quick path alone.
pub const CONTRACT_QUICK: &str = include_str!("../assets/lifecycle-rules-quick.md");
/// `lifecycle/pack.lock.json`: the pack's version and the SHA-256 of each file.
pub const LOCK: &str = include_str!("../../../lifecycle/pack.lock.json");
/// `lifecycle/pack.history.json`: for each pack path, the SHA-256 of every earlier text and the last pack version whose lock held it. It sits
/// outside `lifecycle/pack/`, so the lock does not list it and [`digest`] does not cover it.
pub const HISTORY: &str = include_str!("../../../lifecycle/pack.history.json");
/// `assets/default-rules.history.json`: for `00-my-rules.md`, the SHA-256 of every text the default rules had before the current one and the date
/// that text became the default. Same form as [`HISTORY`] with a date for the label.
pub const DEFAULT_RULES_HISTORY: &str = include_str!("../assets/default-rules.history.json");
/// Where the skills sit in the pack and in a repository.
pub const SKILLS_DIR: &str = ".agents/skills";
/// The kind of skill folder a pack skill has: `lifecycle-<step>`.
pub const SKILL_PREFIX: &str = "lifecycle-";

/// One file of the pack: `path` is relative to `lifecycle/pack/` and `/`-separated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackFile {
    pub path: &'static str,
    pub content: &'static str,
}

macro_rules! pack_file {
    ($path:literal) => {
        PackFile {
            path: $path,
            content: include_str!(concat!("../../../lifecycle/pack/", $path)),
        }
    };
}

/// Every file of the pack, in path order.
pub const FILES: &[PackFile] = &[
    pack_file!(".agents/skills/lifecycle-build-test/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-check/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-check/scripts/lifecycle_check.py"),
    pack_file!(".agents/skills/lifecycle-check/scripts/scanner_kit.py"),
    pack_file!(".agents/skills/lifecycle-context/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-delivery/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-delivery/references/dox-notice.md"),
    pack_file!(".agents/skills/lifecycle-delivery/references/dox-root.md"),
    pack_file!(".agents/skills/lifecycle-loop/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-research/SKILL.md"),
    pack_file!(".agents/skills/lifecycle-tdd/SKILL.md"),
    pack_file!("AGENTS.md"),
];

/// The order the steps run in and the rule number each skill body gets when it is rendered as a rule (`21-lifecycle-research.md` ...).
pub const STEPS: &[(&str, u32)] = &[
    ("research", 21),
    ("context", 22),
    ("tdd", 23),
    ("build-test", 24),
    ("loop", 25),
    ("delivery", 26),
    ("check", 27),
];

/// The pack's files in path order.
pub fn files() -> &'static [PackFile] {
    FILES
}

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The pack's version, from the lock (`1.0.0`).
pub fn version() -> String {
    serde_json::from_str::<serde_json::Value>(LOCK)
        .ok()
        .and_then(|v| {
            v.get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// The pack's digest: SHA-256 of the lines `<path>\t<sha256 of the file>\n` in path order. It names the exact pack a run carried; `ham rules
/// show` and `ham lifecycle init` print it.
pub fn digest() -> String {
    let mut lines = String::new();
    for f in FILES {
        lines.push_str(f.path);
        lines.push('\t');
        lines.push_str(&sha256_hex(f.content.as_bytes()));
        lines.push('\n');
    }
    sha256_hex(lines.as_bytes())
}

/// The text of the contract for a flow: a pure function of the setting (`auto` is [`CONTRACT`]).
pub fn contract_text(flow: LifecycleFlow) -> &'static str {
    match flow {
        LifecycleFlow::Auto => CONTRACT,
        LifecycleFlow::Full => CONTRACT_FULL,
        LifecycleFlow::Quick => CONTRACT_QUICK,
    }
}

/// The contract as the user rule [`CONTRACT_RULE`] (route T0a), in its `auto` text.
pub fn contract_rule() -> RuleFile {
    contract_rule_for(LifecycleFlow::Auto)
}

/// The contract as the user rule [`CONTRACT_RULE`] in the text a flow names (route T0a, `run.lifecycle_flow`).
pub fn contract_rule_for(flow: LifecycleFlow) -> RuleFile {
    rule_file(CONTRACT_RULE, contract_text(flow))
}

/// The skills of the pack, one [`Skill`] per folder, files relative to the folder (route T1).
pub fn skills() -> Vec<Skill> {
    let prefix = format!("{SKILLS_DIR}/");
    let mut out: Vec<Skill> = Vec::new();
    for f in FILES {
        let Some(rest) = f.path.strip_prefix(&prefix) else {
            continue;
        };
        let Some((name, relative)) = rest.split_once('/') else {
            continue;
        };
        let file = SkillFile::new(relative, f.content);
        match out.last_mut() {
            Some(last) if last.name == name => last.files.push(file),
            _ => out.push(Skill {
                name: name.to_string(),
                files: vec![file],
            }),
        }
    }
    out
}

/// Why a skill cannot be rendered as a rule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("skill {0:?} has no SKILL.md")]
    NoEntry(String),
    #[error("skill {0:?}: SKILL.md has no closed frontmatter")]
    NoFrontmatter(String),
    #[error("skill {0:?}: SKILL.md has no body after its frontmatter")]
    EmptyBody(String),
}

/// A skill as one rule file: a heading with the skill's name, its description as one line, then the body, frontmatter removed (route T0b).
pub fn render_skill_rule(skill: &Skill) -> Result<String, RenderError> {
    let name = skill.name.clone();
    let entry = skill
        .files
        .iter()
        .find(|f| f.path == "SKILL.md")
        .ok_or_else(|| RenderError::NoEntry(name.clone()))?;
    let text = entry
        .content
        .strip_prefix("---\n")
        .ok_or_else(|| RenderError::NoFrontmatter(name.clone()))?;
    // The frontmatter ends at the first line that is only three dashes; a body may open with such a line itself.
    let (front, body) = match text.find("\n---\n") {
        Some(i) => (&text[..i], &text[i + "\n---\n".len()..]),
        None => match text.strip_suffix("\n---") {
            Some(front) => (front, ""),
            None => return Err(RenderError::NoFrontmatter(name)),
        },
    };
    let body = body.trim();
    if body.is_empty() {
        return Err(RenderError::EmptyBody(name));
    }
    let description = front
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let mut out = format!("# {}\n\n", skill.name);
    if let Some(d) = description {
        out.push_str(d);
        out.push_str("\n\n");
    }
    out.push_str(body);
    out.push('\n');
    Ok(out)
}

/// The names of the pack's skills (`lifecycle-research` ...), in step order.
pub fn skill_names() -> Vec<String> {
    STEPS
        .iter()
        .map(|(step, _)| format!("{SKILL_PREFIX}{step}"))
        .collect()
}

/// Whether `name` is one of the pack's seven skill names.
pub fn is_skill_name(name: &str) -> bool {
    skill_rule_name(name).is_some()
}

/// The rule file name of a pack skill rendered as a rule: `2n-lifecycle-<step>.md`.
pub fn skill_rule_name(skill_name: &str) -> Option<String> {
    let step = skill_name.strip_prefix(SKILL_PREFIX)?;
    STEPS
        .iter()
        .find(|(s, _)| *s == step)
        .map(|(s, n)| format!("{n}-{SKILL_PREFIX}{s}.md"))
}

/// Every pack skill rendered as a rule, in step order (route T0b). A skill that cannot be rendered is left out; the tests hold the pack to
/// seven.
pub fn skills_as_rules() -> Vec<RuleFile> {
    let skills = skills();
    STEPS
        .iter()
        .filter_map(|(step, _)| {
            let name = format!("{SKILL_PREFIX}{step}");
            let skill = skills.iter().find(|s| s.name == name)?;
            let text = render_skill_rule(skill).ok()?;
            Some(rule_file(&skill_rule_name(&name)?, &text))
        })
        .collect()
}

/// Whether `name` is a rule name this module writes: the contract or a rendered skill.
pub fn is_pack_rule_name(name: &str) -> bool {
    name == CONTRACT_RULE
        || rules::validate_rule_name(name).is_ok() && skill_rule_names().iter().any(|n| n == name)
}

/// The names [`skills_as_rules`] writes.
pub fn skill_rule_names() -> Vec<String> {
    STEPS
        .iter()
        .map(|(step, n)| format!("{n}-{SKILL_PREFIX}{step}.md"))
        .collect()
}

// ---- the earlier texts of the pack's files ------------------------------------------------------------------------------------------

/// The earlier texts of the pack's files: for each pack path (as in [`FILES`]), the SHA-256 of every text that file had before the current
/// one, and the last pack version whose lock held that text. [`History::pack`] is the pack's own ([`HISTORY`]); [`History::parse`] reads
/// any other, so a test can name a text without keeping a copy of the real file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History<'a> {
    files: BTreeMap<&'a str, BTreeMap<&'a str, &'a str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryFile<'a> {
    #[serde(borrow)]
    files: BTreeMap<&'a str, BTreeMap<&'a str, &'a str>>,
}

fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `YYYY-MM-DD`: four digits, a month 01 to 12 and a day 01 to 31, each with its zero.
fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && text[5..7]
            .parse::<u32>()
            .is_ok_and(|m| (1..=12).contains(&m))
        && text[8..10]
            .parse::<u32>()
            .is_ok_and(|d| (1..=31).contains(&d))
}

/// What a history file labels each earlier text with: the name of the file in error messages, the test and the words for it.
struct Labels {
    file: &'static str,
    ok: fn(&str) -> bool,
    what: &'static str,
}

/// The pack's history: the last pack version that held the text.
const PACK_LABELS: Labels = Labels {
    file: "pack history",
    ok: is_version,
    what: "a version X.Y.Z",
};

/// The default rules' history: the date the text became the default.
const DATE_LABELS: Labels = Labels {
    file: "default rules history",
    ok: is_date,
    what: "a date YYYY-MM-DD",
};

impl<'a> History<'a> {
    /// Reads a history text: `{"files": {"<path>": {"<sha256>": "<X.Y.Z>"}}}`. A hash that is not 64 lower-case hexadecimal characters, a
    /// version that is not `X.Y.Z`, a key besides `files` and text that is not JSON are refused.
    pub fn parse(text: &'a str) -> Result<History<'a>, String> {
        History::parse_with(text, &PACK_LABELS)
    }

    /// Reads a history text whose labels are dates, `{"files": {"<path>": {"<sha256>": "<YYYY-MM-DD>"}}}`: the form of the default rules'
    /// history ([`History::default_rules`]). The same refusals as [`History::parse`], with a date where that wants a version.
    pub fn parse_dated(text: &'a str) -> Result<History<'a>, String> {
        History::parse_with(text, &DATE_LABELS)
    }

    fn parse_with(text: &'a str, labels: &Labels) -> Result<History<'a>, String> {
        let raw: HistoryFile<'a> =
            serde_json::from_str(text).map_err(|e| format!("{}: {e}", labels.file))?;
        for (path, hashes) in &raw.files {
            for (digest, label) in hashes {
                if !is_digest(digest) {
                    return Err(format!(
                        "{}: {path}: {digest:?} is not a lower-case SHA-256",
                        labels.file
                    ));
                }
                if !(labels.ok)(label) {
                    return Err(format!(
                        "{}: {path}: {label:?} is not {}",
                        labels.file, labels.what
                    ));
                }
            }
        }
        Ok(History { files: raw.files })
    }

    /// The pack's own history, from [`HISTORY`] (read once). A history that does not parse leaves it empty, so no file is ever replaced
    /// on the strength of a damaged list; the tests hold the embedded file to the parser.
    pub fn pack() -> &'static History<'static> {
        static PACK: OnceLock<History<'static>> = OnceLock::new();
        PACK.get_or_init(|| History::parse(HISTORY).unwrap_or_default())
    }

    /// The default rules' own history, from [`DEFAULT_RULES_HISTORY`] (read once). A history that does not parse leaves it empty, so no file
    /// is ever replaced on the strength of a damaged list; the tests hold the embedded file to the parser.
    pub fn default_rules() -> &'static History<'static> {
        static DEFAULTS: OnceLock<History<'static>> = OnceLock::new();
        DEFAULTS.get_or_init(|| History::parse_dated(DEFAULT_RULES_HISTORY).unwrap_or_default())
    }

    /// The last pack version whose lock held exactly `content` as the text of `path`, when `content` is an earlier text of that path (for the
    /// default rules' history: the date the text became the default).
    pub fn earlier_version(&self, path: &str, content: &[u8]) -> Option<&'a str> {
        let digest = sha256_hex(content);
        self.files.get(path)?.get(digest.as_str()).copied()
    }

    /// The pack paths that have earlier texts, in order.
    pub fn paths(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.files.keys().copied()
    }

    /// The earlier texts of `path` as (SHA-256, version), in hash order; empty for a path with none.
    pub fn hashes(&self, path: &str) -> Vec<(&'a str, &'a str)> {
        self.files
            .get(path)
            .map(|m| m.iter().map(|(d, v)| (*d, *v)).collect())
            .unwrap_or_default()
    }
}

// ---- installing the pack on the user's machine (route T0a/T0b/T1 into the rules and skills folders, T2 into a repository) -------------

/// What an installer did with one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The file did not exist; it was written.
    Created,
    /// The file exists with exactly these bytes.
    Same,
    /// The file exists with other bytes (the user edited it, or the installer may not replace it); it was left alone.
    KeptEdited,
    /// The file held exactly an earlier text of the same pack path ([`History`]; `was` is the last pack version that held it), so nobody
    /// edited it, and it was replaced by the current text.
    Updated { was: String },
}

/// The pack path whose history applies to the rule `name`: the contract rule is `AGENTS.md` of the pack. The other rules (`00-my-rules.md`,
/// the skill bodies of `--skills-as-rules`) have no history.
fn rule_pack_path(name: &str) -> Option<&'static str> {
    (name == CONTRACT_RULE).then_some("AGENTS.md")
}

/// Writes the rule `name` into the rules folder `dir` unless the file is there. The name and the text must make a valid rule (`AGENTS.md`
/// in any case, a bad name and the like are refused before anything is written). An existing file is kept, except the
/// contract rule when it holds exactly a text of the pack's contract: an earlier text ([`History::pack`]) or the text of another flow of this
/// pack version ([`contract_text`]; the user changed `run.lifecycle_flow`). That file was never edited and is replaced.
pub fn install_rule(dir: &Path, name: &str, content: &str) -> Result<Installed, RuleError> {
    validate_rule(name, content)?;
    let io = |e: String| RuleError::new(name, crate::rules::Problem::Unreadable(e));
    std::fs::create_dir_all(dir).map_err(|e| io(e.to_string()))?;
    let path = dir.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(content.as_bytes())
                .map_err(|e| io(e.to_string()))?;
            Ok(Installed::Created)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let have = std::fs::read(&path).map_err(|e| io(e.to_string()))?;
            if have == content.as_bytes() {
                return Ok(Installed::Same);
            }
            // A rule that is a link (a dotfile manager's) is the user's, whatever it holds; only a plain file is replaced.
            let plain = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            if plain
                && name == CONTRACT_RULE
                && let Some(flow) = LifecycleFlow::ALL
                    .into_iter()
                    .find(|f| contract_text(*f).as_bytes() == have.as_slice())
            {
                replace_file(&path, content).map_err(io)?;
                return Ok(Installed::Updated {
                    was: format!("{}, flow {}", version(), flow.word()),
                });
            }
            if plain
                && let Some(pack_path) = rule_pack_path(name)
                && let Some(was) = History::pack().earlier_version(pack_path, &have)
            {
                replace_file(&path, content).map_err(io)?;
                return Ok(Installed::Updated {
                    was: was.to_string(),
                });
            }
            Ok(Installed::KeptEdited)
        }
        Err(e) => Err(io(e.to_string())),
    }
}

/// Brings the user's default rules file `path` (`~/.config/ham/rules/00-my-rules.md`, which must be there) up to date with the current default: a
/// file that holds exactly an earlier default ([`History::default_rules`]) was never edited and is replaced (`Updated`, `was` the date that text
/// became the default); the current text is `Same`; any other file is the user's and is `KeptEdited`. A link (a dotfile manager's) and a folder are
/// the user's whatever they hold, and a link whose target already holds the current text is `Same`. The replacement is atomic and keeps the file's
/// mode (see `replace_file`).
pub fn refresh_default_rules(path: &Path) -> Result<Installed, String> {
    refresh_default_rules_with(History::default_rules(), path, rules::DEFAULT_RULES)
}

/// [`refresh_default_rules`] with the history and the current text given, so a test can name an earlier text without keeping a copy of the real
/// file.
pub fn refresh_default_rules_with(
    history: &History<'_>,
    path: &Path,
    content: &str,
) -> Result<Installed, String> {
    let shown = path.display();
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        let same_through_link = meta.file_type().is_symlink()
            && std::fs::read(path).is_ok_and(|have| have == content.as_bytes());
        return Ok(if same_through_link {
            Installed::Same
        } else {
            Installed::KeptEdited
        });
    }
    let have = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    if have == content.as_bytes() {
        return Ok(Installed::Same);
    }
    if let Some(was) = history.earlier_version(rules::DEFAULT_RULES_NAME, &have) {
        replace_file(path, content)?;
        return Ok(Installed::Updated {
            was: was.to_string(),
        });
    }
    Ok(Installed::KeptEdited)
}

/// What happened to the `.kiro/skills` link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KiroLink {
    Created,
    Same,
    /// Something else is there (a folder, or a link elsewhere); it was left alone. The text says what.
    Kept(String),
}

/// What [`install_repo`] did: one entry per pack file, in path order, and the Kiro link when it was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInstall {
    pub files: Vec<(String, Installed)>,
    pub kiro: Option<KiroLink>,
}

/// The relative target of the Kiro link: `.kiro/skills` is a link to the skills every other engine reads.
const KIRO_TARGET: &str = "../.agents/skills";

/// Makes `rel` (below `root`) and its parents, one folder at a time; a parent that is a link or a file is an error, so nothing is
/// written through a link.
fn make_parents(root: &Path, rel: &Path) -> Result<(), String> {
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(part) = c else {
            return Err(format!("{} is not a plain relative path", rel.display()));
        };
        cur.push(part);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symbolic link; move it away so the pack is not written through it",
                    cur.display()
                ));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} exists and is not a folder", cur.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| format!("{}: {e}", cur.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", cur.display())),
        }
    }
    Ok(())
}

/// Copies the pack into the repository folder `dir` (made if missing): `AGENTS.md` and `.agents/skills/...`, and with `kiro` the link
/// `.kiro/skills`. A file that is already there is kept (`Same` or `KeptEdited`), never overwritten, also when it holds an earlier text of
/// the pack: every file of a repository is the repository's. A link or a folder where a pack file belongs is an error and nothing is
/// written through it.
pub fn install_repo(dir: &Path, kiro: bool) -> Result<RepoInstall, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut done = Vec::new();
    for f in files() {
        let how = place_file(
            dir,
            Path::new(f.path),
            f.content,
            f.path.contains("/scripts/"),
            None,
        )?;
        done.push((f.path.to_string(), how));
    }
    let kiro = if kiro { Some(link_kiro(dir)?) } else { None };
    Ok(RepoInstall { files: done, kiro })
}

/// Writes one file of a skill into the user's skills folder `dir` (`~/.config/ham/skills/`), as `<dir>/<skill>/<path>`. A file that is
/// already there is kept (`Same` or `KeptEdited`) unless it holds exactly an earlier text of the same pack path ([`History::pack`]): that
/// file was never edited and is replaced (`Updated`), atomically and with the mode it had. A missing file is written again; a link on the
/// way (the folder `dir` itself, a skill folder, a parent) or a folder where a file belongs is an error and nothing is written through it.
/// A file below `scripts/` is executable.
pub fn install_skill_file(
    dir: &Path,
    skill: &str,
    path: &str,
    content: &str,
) -> Result<Installed, String> {
    install_skill_file_with(History::pack(), dir, skill, path, content)
}

/// [`install_skill_file`] with the history given.
pub fn install_skill_file_with(
    history: &History<'_>,
    dir: &Path,
    skill: &str,
    path: &str,
    content: &str,
) -> Result<Installed, String> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symbolic link; move it away so the pack is not written through it",
                dir.display()
            ));
        }
        Ok(m) if !m.is_dir() => {
            return Err(format!("{} exists and is not a directory", dir.display()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let pack_path = format!("{SKILLS_DIR}/{skill}/{path}");
    place_file(
        dir,
        &Path::new(skill).join(path),
        content,
        path.starts_with("scripts/"),
        Some((history, &pack_path)),
    )
}

/// One file below `root`: its parents are made one folder at a time (a link or a file among them is an error), an existing file is compared
/// with `content` and kept, a link or a folder where the file belongs is an error, a missing file is written. With `upgrade` (the history and
/// the file's path in the pack) an existing file whose bytes are an earlier text of that path is replaced instead of kept.
fn place_file(
    root: &Path,
    rel: &Path,
    content: &str,
    executable: bool,
    upgrade: Option<(&History<'_>, &str)>,
) -> Result<Installed, String> {
    if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
        make_parents(root, parent)?;
    }
    let path = root.join(rel);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "{} is a symbolic link; move it away so the pack is not written through it",
            path.display()
        )),
        Ok(m) if m.is_file() => {
            let have = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if have == content.as_bytes() {
                return Ok(Installed::Same);
            }
            if let Some((history, pack_path)) = upgrade
                && let Some(was) = history.earlier_version(pack_path, &have)
            {
                replace_file(&path, content)?;
                return Ok(Installed::Updated {
                    was: was.to_string(),
                });
            }
            Ok(Installed::KeptEdited)
        }
        Ok(_) => Err(format!("{} exists and is not a file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_new(&path, content, executable)?;
            Ok(Installed::Created)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_new(path: &Path, content: &str, executable: bool) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if executable { 0o755 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    let mut f = opts
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Replaces the plain file at `path` with `content`: the text goes to a new file in the same folder, which gets the permissions of the old one
/// (a script stays executable) and is renamed over it. At every moment the name holds the old text or the new text, the old inode is never
/// written into, and a failure leaves the old file and removes the new one.
fn replace_file(path: &Path, content: &str) -> Result<(), String> {
    let shown = path.display();
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("{shown} has no folder to write the new text in"));
    };
    let permissions = std::fs::metadata(path)
        .map_err(|e| format!("{shown}: {e}"))?
        .permissions();
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(
            ".{}.ham-new-{}-{attempt}",
            name.to_string_lossy(),
            std::process::id()
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = match opts.open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", tmp.display())),
        };
        let written = file
            .write_all(content.as_bytes())
            .and_then(|()| file.set_permissions(permissions.clone()))
            .and_then(|()| file.sync_all());
        drop(file);
        let done = written.and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = done {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("{shown}: {e}"));
        }
        return Ok(());
    }
    Err(format!(
        "{shown}: no free name for the new file in {}",
        dir.display()
    ))
}

#[cfg(unix)]
fn link_kiro(dir: &Path) -> Result<KiroLink, String> {
    let link = dir.join(".kiro/skills");
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target =
                std::fs::read_link(&link).map_err(|e| format!("{}: {e}", link.display()))?;
            Ok(if target == Path::new(KIRO_TARGET) {
                KiroLink::Same
            } else {
                KiroLink::Kept(format!(
                    "{} is a link to {}, not to {KIRO_TARGET}",
                    link.display(),
                    target.display()
                ))
            })
        }
        Ok(_) => Ok(KiroLink::Kept(format!(
            "{} exists and is not a link; the pack's skills are in .agents/skills",
            link.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            make_parents(dir, Path::new(".kiro"))?;
            std::os::unix::fs::symlink(KIRO_TARGET, &link)
                .map_err(|e| format!("{}: {e}", link.display()))?;
            Ok(KiroLink::Created)
        }
        Err(e) => Err(format!("{}: {e}", link.display())),
    }
}

#[cfg(not(unix))]
fn link_kiro(dir: &Path) -> Result<KiroLink, String> {
    Ok(KiroLink::Kept(format!(
        "links are not made on this system; point {} at .agents/skills yourself",
        dir.join(".kiro/skills").display()
    )))
}
