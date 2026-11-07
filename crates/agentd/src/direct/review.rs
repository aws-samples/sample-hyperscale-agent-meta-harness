//! The review round of a direct run: when the engine ends a turn with a message and the harness counted at least
//! one changed file, the driver sends ONE more prompt in the same session. It asks the engine to review its own delivery as the maintainer who must
//! merge it. This file holds everything about the round that is not the stage loop: the plan the run was given ([`ReviewPlan`]), the rule that
//! decides whether the round runs ([`skip_reason`], pure), the facts the harness computes for the prompt ([`facts_text`], pure; [`ReviewRepo`]
//! reads them from git) and the prompt itself ([`render`], pure). The stage loop (`driver.rs`) sends the prompt, accepts the turn that follows under
//! the rules of any turn, puts the outcome of the turn before the round back when the round failed, and records the round in the summary.
//!
//! The harness decides nothing about the work here: the round is a prompt, the engine's reply is its own words, and the label of the run does not
//! change because of it (root invariant 1).

use crate::BoxFuture;
use crate::direct::classify::{RawEnd, classify};
use crate::workspace::git_with_env;
use harness_protocol::direct::{
    DEFAULT_REVIEW_PROMPT, DirectOutcome, REVIEW_SKIP_BLOCKED, REVIEW_SKIP_CANCELLED,
    REVIEW_SKIP_FILES_UNKNOWN, REVIEW_SKIP_FORCED_STOP, REVIEW_SKIP_NO_CHANGED_FILE,
    REVIEW_SKIP_NO_TIME, REVIEW_SKIP_NOT_DONE, REVIEW_SKIP_OFF, REVIEW_SKIP_QUESTION,
    REVIEW_TOKEN_BASE, REVIEW_TOKEN_FACTS, ReviewMode, ReviewOptions, check_review, head_chars,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long one git read of [`GitRepo`] may take. A repository the engine filled with files that no ignore rule names can make `ls-files` slow,
/// and the round's facts must not hold the run: a read that is not done in this time is "not known".
const GIT_READ_LIMIT: Duration = Duration::from_secs(30);

/// How many changed paths the facts ask the tree for (the lists below are cut shorter).
pub const FACTS_PATHS_ASKED: usize = 200;
/// How many changed paths the facts line lists before "and N more".
const LISTED_PATHS: usize = 12;
/// How many folder or record names a facts line lists.
const LISTED_NAMES: usize = 10;
/// The longest path a facts line shows.
const PATH_CHARS: usize = 160;
/// The folder of the lifecycle pack's records (and the project's own `test-command`).
const RECORDS_DIR: &str = ".ham";
/// The folder of the earlier name (COMPAT-CAH-DATA): a repository that keeps its records there is worked in that folder.
const EARLIER_RECORDS_DIR: &str = crate::direct::records::EARLIER_BASE;

/// What the stage loop needs to run the round of one run. `None` in the wiring means no round (a run without a stage loop).
#[derive(Clone)]
pub struct ReviewPlan {
    /// `Off` when the user or the stage switched the round off: the loop records why it did not run and sends nothing.
    pub mode: ReviewMode,
    /// The prompt with `{{base}}` and `{{facts}}` as typed.
    pub text: String,
    /// The least time the run must have left for the round to start.
    pub guard: Duration,
    /// The commit the run started from.
    pub base: String,
    pub repo: Arc<dyn ReviewRepo>,
}

impl std::fmt::Debug for ReviewPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewPlan")
            .field("mode", &self.mode)
            .field("guard", &self.guard)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl ReviewPlan {
    /// True when the round can run in this run (the stage and the request have not switched it off).
    pub fn is_on(&self) -> bool {
        self.mode != ReviewMode::Off
    }

    /// The plan of a run: the request's `review` member laid over the stage's settings. A run cannot switch on what the stage switched off.
    pub fn new(
        request: Option<&ReviewOptions>,
        stage_on: bool,
        stage_guard: Duration,
        base: &str,
        repo: Arc<dyn ReviewRepo>,
    ) -> Self {
        let off = !stage_on || request.is_some_and(|r| r.mode == ReviewMode::Off);
        Self {
            mode: if off {
                ReviewMode::Off
            } else {
                ReviewMode::Auto
            },
            text: request
                .and_then(|r| r.text.clone())
                .unwrap_or_else(|| DEFAULT_REVIEW_PROMPT.to_string()),
            guard: request
                .and_then(|r| r.guard_s)
                .map_or(stage_guard, |s| Duration::from_secs(u64::from(s))),
            base: base.to_string(),
            repo,
        }
    }
}

/// The `review` member of the stored request, checked again with the one check `ham` and control-api call (agentd trusts neither a stale client
/// nor a hand-made request). A run with no member has none to check. The message names the member and the rule, never the value.
pub fn check_member(review: Option<&ReviewOptions>) -> Result<(), String> {
    match review {
        Some(r) => check_review(r).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

// ---- the work tree's git facts --------------------------------------------------------------------------------------------------

/// The git facts of the work tree that the work-tree measurer ([`crate::direct::input::TreeSource`]) does not give. Each answers `None` when it
/// cannot be read: unknown is never read as zero, and the facts say "not known".
pub trait ReviewRepo: Send + Sync {
    /// The commit `HEAD` points to now (40 or 64 lowercase hex digits).
    fn head(&self) -> BoxFuture<'_, Option<String>>;
    /// The names at the top of the base commit's tree.
    fn base_top_level(&self) -> BoxFuture<'_, Option<Vec<String>>>;
    /// How many files the work tree has that git neither tracks nor ignores.
    fn untracked(&self) -> BoxFuture<'_, Option<u32>>;
}

fn is_long_hex(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The real reader: three read-only git calls in the engine's repository, with agentd's guards (hooks, fsmonitor and the global configuration
/// off). None of them hashes file content, so no filter the engine may have written runs: `rev-parse`, `ls-tree` and `ls-files --others` only
/// read names.
#[derive(Debug)]
pub struct GitRepo {
    repo: PathBuf,
    base: String,
}

impl GitRepo {
    pub fn new(repo: &Path, base: &str) -> Self {
        Self {
            repo: repo.to_path_buf(),
            base: base.to_string(),
        }
    }
}

impl ReviewRepo for GitRepo {
    fn head(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let head = tokio::time::timeout(
                GIT_READ_LIMIT,
                git_with_env(&self.repo, &["rev-parse", "HEAD"], &[]),
            )
            .await
            .ok()?
            .ok()?;
            is_long_hex(&head).then_some(head)
        })
    }

    fn base_top_level(&self) -> BoxFuture<'_, Option<Vec<String>>> {
        Box::pin(async move {
            // The base is an object id the driver read itself; a value that is not one never reaches git as an option or a name.
            if !is_long_hex(&self.base) {
                return None;
            }
            let out = tokio::time::timeout(
                GIT_READ_LIMIT,
                git_with_env(
                    &self.repo,
                    &["ls-tree", "--name-only", "-z", &self.base],
                    &[],
                ),
            )
            .await
            .ok()?
            .ok()?;
            Some(
                out.split('\0')
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        })
    }

    fn untracked(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move {
            let out = tokio::time::timeout(
                GIT_READ_LIMIT,
                git_with_env(
                    &self.repo,
                    &["ls-files", "--others", "--exclude-standard", "-z"],
                    &[],
                ),
            )
            .await
            .ok()?
            .ok()?;
            let n = out.split('\0').filter(|n| !n.is_empty()).count();
            Some(u32::try_from(n).unwrap_or(u32::MAX))
        })
    }
}

// ---- the rule: does the round run? ----------------------------------------------------------------------------------------------

/// Everything the rule reads.
#[derive(Debug)]
pub struct Gate<'a> {
    pub mode: ReviewMode,
    /// A module of the seam (the goal loop's brake) ended the run.
    pub hook_stop: bool,
    /// The user's cancel is up: the run is ending as the user's stop, and no prompt goes out.
    pub cancelled: bool,
    /// How the last turn ended.
    pub raw: &'a RawEnd,
    /// The changed files the harness counted; `None` when it could not count.
    pub files: Option<u32>,
    /// What is left of the run's time.
    pub left: Duration,
    pub guard: Duration,
}

/// Why the round does not run, as the `DirectReview.reason` word; `None` when it runs. The first rule that applies wins, in this order: the
/// round is off; the harness ended the run; the user cancelled; the last turn did not end with a message from the engine; a line of the message starts with `BLOCKED:`
/// (any line, not only the last, so a quoted example also stops the round, which is the safe side); the last line of the message that holds a
/// letter or a digit ends with a question mark; the harness could not count the changed files, or counted none; less than the guard is left.
/// The label "reported done" and this rule share their condition (a message and at least one changed file), so a round never runs for a run that
/// would not have that label.
pub fn skip_reason(g: &Gate<'_>) -> Option<&'static str> {
    if g.mode == ReviewMode::Off {
        return Some(REVIEW_SKIP_OFF);
    }
    if g.hook_stop {
        return Some(REVIEW_SKIP_FORCED_STOP);
    }
    if g.cancelled {
        return Some(REVIEW_SKIP_CANCELLED);
    }
    if classify(g.raw).outcome != DirectOutcome::ReportedDone {
        return Some(REVIEW_SKIP_NOT_DONE);
    }
    if says_blocked(&g.raw.message) {
        return Some(REVIEW_SKIP_BLOCKED);
    }
    if asks_the_user(&g.raw.message) {
        return Some(REVIEW_SKIP_QUESTION);
    }
    match g.files {
        None => return Some(REVIEW_SKIP_FILES_UNKNOWN),
        Some(0) => return Some(REVIEW_SKIP_NO_CHANGED_FILE),
        Some(_) => {}
    }
    if g.left < g.guard {
        return Some(REVIEW_SKIP_NO_TIME);
    }
    None
}

/// A line of the message, without the marks a model puts before text (list dashes, quote marks, heading hashes, bold and code marks).
fn bare(line: &str) -> &str {
    line.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '_' | '`' | '>' | '#' | '-' | '"' | '\'')
    })
}

/// True when a line of the message starts with `BLOCKED:` (the way the lifecycle contract ends a run that cannot go on). Any line counts, not only
/// the last: a message that reports the block and then lists what it did is still a stop.
pub fn says_blocked(message: &str) -> bool {
    message.lines().any(|l| {
        // Emphasis marks inside the first words (`**BLOCKED**:`) do not hide the sentinel.
        let plain: String = bare(l)
            .chars()
            .filter(|c| !matches!(c, '*' | '_' | '`'))
            .collect();
        plain.starts_with("BLOCKED:")
    })
}

/// True when the last line of the message that holds text ends with a question mark: the engine stopped to ask the user something, which a review
/// of the delivery would not answer. A line with no letter and no digit (a closing code fence, a rule, a bare mark) is not text. A heuristic by
/// design: the harness reads the shape of the message, never its meaning, so a closing rhetorical question stops the round too.
pub fn asks_the_user(message: &str) -> bool {
    let last = message
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| l.chars().any(char::is_alphanumeric));
    last.is_some_and(|l| {
        let ends = l
            .trim_end_matches(|c: char| {
                c.is_whitespace() || matches!(c, '*' | '_' | '`' | '"' | '\'' | ')' | ']')
            })
            .ends_with(['?', '\u{ff1f}']);
        ends && !is_offer(l)
    })
}

/// A closing offer after finished work ("Let me know if you want X?", "Would you like me to add Y?"): it asks for nothing the delivery needs, so it
/// does not stop the round. The openers are few and exact; any other question stops it.
fn is_offer(line: &str) -> bool {
    const OPENERS: [&str; 7] = [
        "let me know",
        "would you like me to",
        "want me to",
        "do you want me to",
        "if you'd like",
        "if you would like",
        "feel free to",
    ];
    let lower: String = bare(line)
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`'))
        .collect::<String>()
        .to_lowercase();
    OPENERS.iter().any(|o| lower.starts_with(o))
}

// ---- the facts and the prompt ---------------------------------------------------------------------------------------------------

/// What [`facts_text`] reads.
#[derive(Debug)]
pub struct FactsInput<'a> {
    /// The first changed paths the tree gave (at most [`FACTS_PATHS_ASKED`]).
    pub paths: &'a [String],
    /// How many files differ from the base in all.
    pub total: u32,
    /// The names at the top of the base tree; `None` when git could not say.
    pub base_top_level: Option<&'a [String]>,
    /// Files git neither tracks nor ignores; `None` when git could not say.
    pub untracked: Option<u32>,
}

/// One path as a facts line shows it: on one line, with no control character, cut.
fn shown(path: &str) -> String {
    let one_line: String = path
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    head_chars(&one_line, PATH_CHARS).to_string()
}

fn listed(items: &[String], max: usize) -> String {
    let mut out = items
        .iter()
        .take(max)
        .map(|p| shown(p))
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > max {
        out.push_str(&format!(", and {} more", items.len() - max));
    }
    out
}

/// The top-level folders the changed paths create: the first part of a path that has a folder in it and is not a name at the top of the base tree.
fn new_top_level_folders(paths: &[String], base: &[String]) -> Vec<String> {
    let mut folders: Vec<String> = paths
        .iter()
        .filter_map(|p| p.split_once('/').map(|(top, _)| top))
        .filter(|top| {
            !top.is_empty()
                && *top != RECORDS_DIR
                && *top != EARLIER_RECORDS_DIR
                && !base.iter().any(|b| b == top)
        })
        .map(|top| format!("{top}/"))
        .collect();
    folders.sort();
    folders.dedup();
    folders
}

/// The paths of the lifecycle pack's records (and the project's own files) under `.ham`, or under the earlier folder.
fn record_files(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| under(p, RECORDS_DIR) || under(p, EARLIER_RECORDS_DIR))
        .cloned()
        .collect()
}

fn under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The folder the facts line names: the earlier folder when the changed records are all there, else `.ham`.
fn records_label(records: &[String]) -> &'static str {
    if !records.is_empty() && records.iter().all(|p| under(p, EARLIER_RECORDS_DIR)) {
        EARLIER_RECORDS_DIR
    } else {
        RECORDS_DIR
    }
}

/// The lines the prompt's `{{facts}}` stands for: what the harness computed from the work tree, one fact to a line, each line starting with a
/// dash. Plain words and counts; no claim about the work.
pub fn facts_text(i: &FactsInput<'_>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let cut = (i.paths.len() as u64) < u64::from(i.total);
    let from = if cut {
        format!(" (from the first {} changed paths)", i.paths.len())
    } else {
        String::new()
    };
    if i.paths.is_empty() {
        lines.push(format!("- Changed files: {}.", i.total));
    } else {
        lines.push(format!(
            "- Changed files: {} ({}).",
            i.total,
            listed(i.paths, LISTED_PATHS)
        ));
    }
    // Paths the tree could not list while files differ say nothing about folders or records.
    let known = !i.paths.is_empty() || i.total == 0;
    lines.push(match i.base_top_level.filter(|_| known) {
        Some(base) => {
            let folders = new_top_level_folders(i.paths, base);
            if folders.is_empty() {
                format!("- New top-level folders: none{from}.")
            } else {
                format!(
                    "- New top-level folders: {}{from}.",
                    listed(&folders, LISTED_NAMES)
                )
            }
        }
        None => "- New top-level folders: not known.".to_string(),
    });
    let records = record_files(i.paths);
    let folder = records_label(&records);
    lines.push(if !known {
        format!("- Files under {folder}/ (records the harness reads; keep them): not known.")
    } else if records.is_empty() {
        format!("- Files under {folder}/ (records the harness reads; keep them): none{from}.")
    } else {
        format!(
            "- Files under {folder}/ (records the harness reads; keep them): {}{from}.",
            listed(&records, LISTED_NAMES)
        )
    });
    lines.push(match i.untracked {
        Some(n) => format!("- Untracked files: {n}."),
        None => "- Untracked files: not known.".to_string(),
    });
    lines.join("\n")
}

/// The prompt: `template` with `{{base}}` replaced by the commit the run started from and `{{facts}}` by the facts. One left-to-right pass, so
/// text that was put in is never read for a token again (a path named `{{base}}` stays a path). A template with neither token is sent as it is.
pub fn render(template: &str, base: &str, facts: &str) -> String {
    let mut out = String::with_capacity(template.len() + facts.len() + base.len());
    let mut rest = template;
    loop {
        let next = match (rest.find(REVIEW_TOKEN_BASE), rest.find(REVIEW_TOKEN_FACTS)) {
            (None, None) => break,
            (Some(b), None) => (b, REVIEW_TOKEN_BASE, base),
            (None, Some(f)) => (f, REVIEW_TOKEN_FACTS, facts),
            (Some(b), Some(f)) if b <= f => (b, REVIEW_TOKEN_BASE, base),
            (Some(_), Some(f)) => (f, REVIEW_TOKEN_FACTS, facts),
        };
        out.push_str(&rest[..next.0]);
        out.push_str(next.2);
        rest = &rest[next.0 + next.1.len()..];
    }
    out.push_str(rest);
    out
}
