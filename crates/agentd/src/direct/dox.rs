//! The DOX starter is written into the work tree and committed before the first prompt of a run.
//!
//! The engine builds its rules text from the root `AGENTS.md` when its session starts. A root file created later in the run reaches no request of
//! that run, and a run that ends early, or is cut at its time limit, used to leave no docs tree. So the harness writes the starter first and the
//! engine loads the DOX rules from its first request. The harness adds no check, no gate and no setting: the engine and the model keep the docs.
//! The user's way out stays: `DOX off` as the first line of their copy of the starter, a request that says to write
//! no AGENTS.md, and a run with no skills (`--no-skills`, `--no-rules`) has no starter to write.
//!
//! What is written is the user's own file: `references/dox-root.md` of the skill `lifecycle-delivery`, which `ham` sends in the request's `skills`
//! member, and, when the starter holds the DOX framework, its notice `references/dox-notice.md` as `THIRD-PARTY-NOTICES.md`. The text is never
//! interpreted here.
//!
//! The commit comes before the run's base commit is read ([`settle`]), so the starter is not the run's own change: the count of changed files,
//! the "no changes" label, the review round and the summary all measure from it. The commit is built with git plumbing only (a blob without
//! filters, a private index, a tree, a commit with the parent's own commit time and the harness as author), so the same inputs give the same
//! commit and no program a repository names can run. It is made only in a checkout this invocation just cloned, on a head with no work on it:
//! the first slice, and a later slice that restores the original bundle (nothing was saved). A later slice that restores its predecessor's work
//! finds the commit in the restored history: the service names the head of the original bundle as the run's base, and the commit the harness
//! would make on it is the base when it is an ancestor of the restored head, else that original head. A commit that only looks like it (the same
//! subject) is not taken for it: the commit is recomputed, never searched for.
//!
//! Best effort: a starter that cannot be written safely (a link, an ignored path, a changed index, a refused git call) leaves the tree as it was
//! and the run goes on without it; the delivery skill still writes the root at the end of the run when none exists. Only an undo that fails ends
//! the run before the prompt, because the work tree is then not known.

use crate::workspace::{self, Worktree, WsError};
use harness_protocol::direct::{Skill, check_skills};
use regex::Regex;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// The subject of the commit that holds the starter.
pub const SEED_SUBJECT: &str = "harness: DOX starter";

/// The skill that carries the starter, and the files of it that are read.
pub const SKILL: &str = "lifecycle-delivery";
const ROOT_FILE: &str = "references/dox-root.md";
const NOTICE_FILE: &str = "references/dox-notice.md";

/// The files the commit holds.
const ROOT_PATH: &str = "AGENTS.md";
const NOTICE_PATH: &str = "THIRD-PARTY-NOTICES.md";

/// The heading that marks a root file as a DOX root, and the first line of a root `ham lifecycle init` wrote.
const FRAMEWORK_LINE: &str = "# DOX framework";
const LIFECYCLE_LINE: &str = "# Development lifecycle";

/// The most bytes of an existing root or notice file the harness reads to decide.
const MAX_TRACKED_BYTES: u64 = 1 << 20;

/// The message of the commit. It is a constant: the commit is a function of the head it sits on and of the user's starter.
const MESSAGE: &str = "harness: DOX starter\n\n\
The harness wrote the root AGENTS.md (the DOX rules for the docs of this project) and the notice THIRD-PARTY-NOTICES.md before the first prompt \
of the run, from the starter in your lifecycle-delivery skill (references/dox-root.md). The text is yours: the engine reads it from its first \
request and keeps the docs current as it works. `DOX off` as the first line of that file switches this off.\n";

/// The user's starter and its notice, from the delivery skill of the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Starter {
    pub root: String,
    pub notice: Option<String>,
}

/// The starter the request carries: the file `references/dox-root.md` of the skill `lifecycle-delivery`, and its notice when the skill has one.
/// `None` for a request with no skills, with other skills only, with no such file, with a blank one, or whose skills the shared check refuses
/// (the `skills` module ends that run at its own startup).
pub fn starter(skills: &[Skill]) -> Option<Starter> {
    if skills.is_empty() || check_skills(skills).is_err() {
        return None;
    }
    let skill = skills.iter().find(|s| s.name == SKILL)?;
    let text = |path: &str| {
        skill
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.content.clone())
            .filter(|t| !t.trim().is_empty())
    };
    Some(Starter {
        root: text(ROOT_FILE)?,
        notice: text(NOTICE_FILE),
    })
}

/// The ban of the docs in the user's request, in the words of the pack's own self-check.
/// The list is short on purpose: "DOX off", "no DOX", "no
/// framework" and "no docs" are no prohibition of the file, and a sentence that only says there is none ("The repository has no AGENTS.md yet.") is
/// not one. Five forms: a refusal and a verb, a verb or a report of the request and "no", "without", and "no" that begins a sentence, a clause, a
/// quotation or a list item. Between the word and the file name only "a", "an", "any", "the" and "root" may stand. Only the user's request is read:
/// the rule files carry the lifecycle contract, which names the opt-out in the same words. A ban in other words is for the model to follow: the
/// delivery skill takes the starter out again.
static BAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?mi)(?:\b(?:do not|don't|never)\s+(?:write|create|add|make|generate|include)"#,
        r#"|\b(?:write|create|add|make|generate|include|requested|asked for|wants?)\s+no"#,
        r#"|\bwithout"#,
        r#"|(?:^|[.;:!?\n("'`*-])\s*no)"#,
        r#"\s+(?:(?:a|an|any|the|root)\s+)*agents\.md\b"#
    ))
    .expect("the ban pattern is a valid regular expression")
});

/// Whether the request's text bans the docs ([`BAN`]).
pub fn bans(prompt: &str) -> bool {
    BAN.is_match(prompt)
}

/// Whether the starter switches the docs off for every project: its first line is `DOX off` (a byte order mark, spaces, a trailing carriage return
/// and the case of the letters do not matter; nothing else on the line, and nothing else of the file, does).
pub fn is_off(root: &str) -> bool {
    let first = root.trim_start_matches('\u{feff}').lines().next();
    first.is_some_and(|l| l.trim().eq_ignore_ascii_case("DOX off"))
}

/// Why nothing was written. The words go to agentd's log, never to the run's event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    NoStarter,
    ContinueRun,
    Off,
    Banned,
    HasFramework,
    OwnText,
    NotAFile,
    Ignored,
    ReusedCheckout,
    NotClean(String),
    Failed(String),
}

impl Why {
    fn words(&self) -> String {
        match self {
            Self::NoStarter => "the request carries no starter".to_string(),
            Self::ContinueRun => {
                "a continue run starts on the head the earlier run delivered".to_string()
            }
            Self::Off => "the first line of the starter is DOX off".to_string(),
            Self::Banned => "the request says to write no AGENTS.md".to_string(),
            Self::HasFramework => "the root file already holds the DOX framework".to_string(),
            Self::OwnText => "the root file is the user's own".to_string(),
            Self::NotAFile => "AGENTS.md is not a plain file of the repository".to_string(),
            Self::Ignored => "the repository ignores AGENTS.md".to_string(),
            Self::ReusedCheckout => "the checkout was made by an earlier invocation".to_string(),
            Self::NotClean(what) => format!("the checkout is not as the bundle left it: {what}"),
            Self::Failed(what) => format!("not written, tree put back: {what}"),
        }
    }
}

/// A path as the head the run restored tracks it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tracked {
    Absent,
    /// A regular file: its mode, its blob and its text (`None`: not UTF-8 or too big to read here).
    File {
        mode: String,
        text: Option<String>,
    },
    /// A link, a submodule, a folder.
    Other,
}

/// One file the commit adds or extends.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Put {
    path: &'static str,
    mode: String,
    /// What the file holds after the commit.
    text: String,
    /// What it held before (the extended file), `None` for a new file.
    was: Option<String>,
}

/// `existing` with `add` below it, a blank line between.
fn below(existing: &str, add: &str) -> String {
    let mut s = existing.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push('\n');
    s.push_str(add);
    s
}

fn holds_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l.trim_end() == line)
}

/// A tracked file with no text in it (nothing, white space, a byte order mark alone). The project's own tools read it as no root
/// file.
fn is_blank(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim().is_empty()
}

/// What to write, from what the head tracks. `Err` is the reason there is nothing to write. A root with no text counts as none: the starter replaces it.
fn decide(
    starter: &Starter,
    prompt: &str,
    root: &Tracked,
    notice: &Tracked,
) -> Result<Vec<Put>, Why> {
    if is_off(&starter.root) {
        return Err(Why::Off);
    }
    if bans(prompt) {
        return Err(Why::Banned);
    }
    let mut puts = Vec::new();
    match root {
        Tracked::Absent => puts.push(Put {
            path: ROOT_PATH,
            mode: "100644".to_string(),
            text: starter.root.clone(),
            was: None,
        }),
        Tracked::File {
            mode,
            text: Some(text),
        } if is_blank(text) => puts.push(Put {
            path: ROOT_PATH,
            mode: mode.clone(),
            text: starter.root.clone(),
            was: Some(text.clone()),
        }),
        Tracked::File {
            mode,
            text: Some(text),
        } => {
            if holds_line(text, FRAMEWORK_LINE) {
                return Err(Why::HasFramework);
            }
            if text
                .trim_start_matches('\u{feff}')
                .lines()
                .next()
                .map(str::trim_end)
                != Some(LIFECYCLE_LINE)
            {
                return Err(Why::OwnText);
            }
            puts.push(Put {
                path: ROOT_PATH,
                mode: mode.clone(),
                text: below(text, &starter.root),
                was: Some(text.clone()),
            });
        }
        Tracked::File { text: None, .. } | Tracked::Other => return Err(Why::NotAFile),
    }
    // The notice goes with the framework text, as the delivery skill writes it: a new file, or below the text of one that does not name DOX.
    if let Some(text) = starter
        .notice
        .as_deref()
        .filter(|_| holds_line(&starter.root, FRAMEWORK_LINE))
    {
        match notice {
            Tracked::Absent => puts.push(Put {
                path: NOTICE_PATH,
                mode: "100644".to_string(),
                text: text.to_string(),
                was: None,
            }),
            Tracked::File {
                mode,
                text: Some(existing),
            } if !existing.contains("DOX") => puts.push(Put {
                path: NOTICE_PATH,
                mode: mode.clone(),
                text: below(existing, text),
                was: Some(existing.clone()),
            }),
            _ => {}
        }
    }
    Ok(puts)
}

/// What [`settle`] needs from the invocation.
#[derive(Debug)]
pub struct Request<'a> {
    pub skills: &'a [Skill],
    /// The user's prompt as typed (the request's `prompt`, never a slice's continuation prompt).
    pub prompt: &'a str,
    /// A `ham continue` run: it starts on a head that must be the one the earlier run delivered, so nothing is added to it.
    pub continue_run: bool,
    /// The run's folder under agentd's private data directory (the private index is made there, never in the work tree).
    pub agent_data: &'a Path,
}

/// The base commit of the run's own work, and what was done about the starter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub base: String,
    /// One line for agentd's log, when a starter was in play.
    pub note: Option<String>,
}

fn settled(base: &str, note: Option<String>) -> Settled {
    Settled {
        base: base.to_string(),
        note,
    }
}

fn skipped(base: &str, why: &Why) -> Settled {
    let note = match why {
        Why::NoStarter => None,
        other => Some(format!("not written: {}", other.words())),
    };
    settled(base, note)
}

fn git_err(e: &WsError) -> Why {
    Why::Failed(e.to_string())
}

/// The base commit of the run's work, after the starter commit is made when it is due.
///
/// `origin` is the head the run started from: the head of the restored bundle in the first slice, and the original base the service names in a later
/// one. `fresh` is true when this invocation just cloned the checkout. The three results:
///
/// * the checkout is at `origin`, was just cloned and takes the starter: the commit is made on it and is the base;
/// * the checkout holds work on top of `origin` (a later slice): the base is the commit the harness would have made on `origin` when that commit
///   is in the restored history, else `origin`;
/// * anything else (no starter, a skip rule, a reused checkout, a refused git call): `origin`.
///
/// `Err` only when an undo failed, so the work tree is not known; the run does not start.
pub async fn settle(
    wt: &Worktree,
    req: &Request<'_>,
    origin: &str,
    fresh: bool,
) -> Result<Settled, String> {
    let Some(starter) = starter(req.skills) else {
        return Ok(skipped(origin, &Why::NoStarter));
    };
    if req.continue_run {
        return Ok(skipped(origin, &Why::ContinueRun));
    }
    if let Err(e) = workspace::check_git_dir(&wt.repo) {
        return Ok(skipped(origin, &git_err(&e)));
    }
    let puts = match tracked_and_decide(wt, &starter, req.prompt, origin).await {
        Ok(p) => p,
        Err(why) => return Ok(skipped(origin, &why)),
    };
    let seed = match build_commit(wt, req.agent_data, origin, &puts).await {
        Ok(s) => s,
        Err(why) => return Ok(skipped(origin, &why)),
    };
    let head = match git_text(&wt.repo, &["rev-parse", "HEAD"], &[], None).await {
        Ok(h) => h,
        Err(e) => return Ok(skipped(origin, &git_err(&e))),
    };
    if head != origin {
        // Work on top of the origin: a later slice. The commit is in its history or it is not; nothing is made.
        let answer = git_run(
            &wt.repo,
            &["merge-base", "--is-ancestor", &seed, &head],
            &[],
            None,
        )
        .await;
        return Ok(match answer.map(|o| o.status.code()) {
            Ok(Some(0)) => settled(
                &seed,
                Some(format!(
                    "the run's base is the starter commit {}",
                    short(&seed)
                )),
            ),
            Ok(Some(1)) => settled(origin, None),
            Ok(other) => settled(
                origin,
                Some(format!(
                    "the starter commit could not be placed in the restored history (git exited {other:?})"
                )),
            ),
            Err(e) => settled(
                origin,
                Some(format!(
                    "the starter commit could not be placed in the restored history: {e}"
                )),
            ),
        });
    }
    if !fresh {
        return Ok(skipped(origin, &Why::ReusedCheckout));
    }
    match apply(wt, origin, &seed, &puts).await {
        Ok(Ok(())) => Ok(settled(
            &seed,
            Some(format!("written, commit {}", short(&seed))),
        )),
        Ok(Err(why)) => Ok(skipped(origin, &why)),
        Err(fatal) => Err(fatal),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// What the head tracks at the root path `path`. A tracked name that differs from it only in the case of its letters counts as `Other`: the
/// project would hold two names that collide on a checkout that folds case.
async fn tracked(wt: &Worktree, head: &str, path: &str) -> Result<Tracked, Why> {
    let listing = git_bytes(&wt.repo, &["ls-tree", "-z", head])
        .await
        .map_err(|e| git_err(&e))?;
    // `<mode> <type> <blob>\t<name>` for each entry of the root
    let mut found: Option<(String, String, String)> = None;
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((meta, name)) = entry.split_once('\t') else {
            continue;
        };
        if !name.eq_ignore_ascii_case(path) {
            continue;
        }
        if name != path {
            return Ok(Tracked::Other);
        }
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(blob)) = (parts.next(), parts.next(), parts.next())
        else {
            return Ok(Tracked::Other);
        };
        found = Some((mode.to_string(), kind.to_string(), blob.to_string()));
    }
    let Some((mode, kind, blob)) = found else {
        return Ok(Tracked::Absent);
    };
    if kind != "blob" || !matches!(mode.as_str(), "100644" | "100755") {
        return Ok(Tracked::Other);
    }
    let size = git_text(&wt.repo, &["cat-file", "-s", &blob], &[], None)
        .await
        .map_err(|e| git_err(&e))?;
    let text = if size.parse::<u64>().is_ok_and(|n| n <= MAX_TRACKED_BYTES) {
        let bytes = git_bytes(&wt.repo, &["cat-file", "blob", &blob])
            .await
            .map_err(|e| git_err(&e))?;
        String::from_utf8(bytes).ok()
    } else {
        None
    };
    Ok(Tracked::File { mode, text })
}

async fn tracked_and_decide(
    wt: &Worktree,
    starter: &Starter,
    prompt: &str,
    head: &str,
) -> Result<Vec<Put>, Why> {
    // The cheap rules first: they need no git call.
    if is_off(&starter.root) {
        return Err(Why::Off);
    }
    if bans(prompt) {
        return Err(Why::Banned);
    }
    let root = tracked(wt, head, ROOT_PATH).await?;
    let notice = tracked(wt, head, NOTICE_PATH).await?;
    decide(starter, prompt, &root, &notice)
}

// ---- the commit ---------------------------------------------------------------------------------------------------------------------

/// Builds the commit with plumbing only: no file of the work tree is read through a filter, no hook, no attribute and no setting of the repository
/// is consulted. The result is a function of `origin`, of its commit time and of `puts`.
async fn build_commit(
    wt: &Worktree,
    agent_data: &Path,
    origin: &str,
    puts: &[Put],
) -> Result<String, Why> {
    let index = index_path(agent_data);
    let result = build_commit_in(wt, agent_data, &index, origin, puts).await;
    let _ = std::fs::remove_file(&index);
    result.map_err(|e| git_err(&e))
}

async fn build_commit_in(
    wt: &Worktree,
    agent_data: &Path,
    index: &Path,
    origin: &str,
    puts: &[Put],
) -> Result<String, WsError> {
    let repo = wt.repo.as_path();
    let _ = std::fs::remove_file(index);
    git_text(repo, &["read-tree", origin], &[], Some(index)).await?;
    for (i, put) in puts.iter().enumerate() {
        let blob = agent_data.join(format!("dox-starter-{i}.blob"));
        // A file a killed run left is agentd's own: it goes before the new one is made, and again whatever happens next.
        let _ = std::fs::remove_file(&blob);
        let made = match write_new(&blob, put.text.as_bytes()) {
            Ok(()) => {
                git_text(
                    repo,
                    &[
                        "hash-object",
                        "-w",
                        "--no-filters",
                        &blob.display().to_string(),
                    ],
                    &[],
                    None,
                )
                .await
            }
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_file(&blob);
        let entry = format!("{},{},{}", put.mode, made?, put.path);
        git_text(
            repo,
            &["update-index", "--add", "--cacheinfo", &entry],
            &[],
            Some(index),
        )
        .await?;
    }
    let tree = git_text(repo, &["write-tree"], &[], Some(index)).await?;
    // The commit time of the head the commit sits on, read from the raw object (no setting of the repository can change how it is shown): a fixed
    // value, so the same inputs give the same commit in every session.
    let raw = git_bytes(repo, &["cat-file", "commit", origin]).await?;
    let when = committer_time(&raw)
        .ok_or_else(|| WsError::Git("cat-file: the head has no committer time".to_string()))?;
    let date = format!("{when} +0000");
    git_text(
        repo,
        &[
            "-c",
            "i18n.commitEncoding=UTF-8",
            "commit-tree",
            &tree,
            "-p",
            origin,
            "-m",
            MESSAGE,
        ],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
        None,
    )
    .await
}

/// The Unix time in the `committer` line of a raw commit object (`committer Name <mail> <seconds> <zone>`).
fn committer_time(raw: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(raw);
    let header = text.split("\n\n").next()?;
    let line = header.lines().find(|l| l.starts_with("committer "))?;
    let mut tail = line.rsplit(' ');
    let _zone = tail.next()?;
    tail.next()?.parse().ok()
}

/// A new private file; it must not be there.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), WsError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| WsError::Io(e.to_string()))?;
    f.write_all(bytes).map_err(|e| WsError::Io(e.to_string()))
}

/// Puts the commit on the checkout: the files, the index entries and, last, the branch. `Ok(Err(why))` is a refusal with the tree as it was;
/// `Err` is an undo that failed. It runs only in a checkout this invocation just cloned, so the repository's own settings are the clone's.
async fn apply(
    wt: &Worktree,
    origin: &str,
    seed: &str,
    puts: &[Put],
) -> Result<Result<(), Why>, String> {
    let repo = wt.repo.as_path();
    if let Err(why) = preconditions(wt, puts).await {
        return Ok(Err(why));
    }
    // 1. the files, each without following a link. A file counts as touched once it is open: a write that fails after the open leaves a part.
    let mut touched: Vec<&Put> = Vec::new();
    for put in puts {
        match write_file(repo, put) {
            Ok(()) => touched.push(put),
            Err(e) => {
                if e.opened {
                    touched.push(put);
                }
                undo_files(repo, &touched)?;
                return Ok(Err(Why::Failed(e.why)));
            }
        }
    }
    // 2. the index of the checkout: the tree of the commit. `read-tree` runs no clean filter; `update-index --cacheinfo` does, for a path whose file is
    // already in the work tree and a filter that the repository's configuration names. The index was equal to the head's tree (preconditions), so the
    // entries other than the starter's are the ones it had; only their stat data is read again by the next `git status`.
    if let Err(e) = git_text(repo, &["read-tree", seed], &[], None).await {
        return undone(repo, origin, &touched, git_err(&e)).await;
    }
    // 3. the branch, last: a compare-and-swap on the head the checkout is at
    let branch = format!("refs/heads/{}", wt.branch);
    if let Err(e) = git_text(
        repo,
        &["update-ref", "-m", SEED_SUBJECT, &branch, seed, origin],
        &[],
        None,
    )
    .await
    {
        return undone(repo, origin, &touched, git_err(&e)).await;
    }
    Ok(Ok(()))
}

/// Both undos run whatever the first one says; a failure of either is the fatal answer, because the tree is then not known.
async fn undone(
    repo: &Path,
    origin: &str,
    touched: &[&Put],
    why: Why,
) -> Result<Result<(), Why>, String> {
    let index = undo_index(repo, origin).await;
    let files = undo_files(repo, touched);
    match (index, files) {
        (Ok(()), Ok(())) => Ok(Err(why)),
        (a, b) => Err([a.err(), b.err()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")),
    }
}

/// The checkout is as the bundle left it, and the paths may be written.
async fn preconditions(wt: &Worktree, puts: &[Put]) -> Result<(), Why> {
    let repo = wt.repo.as_path();
    let head_ref = git_text(repo, &["symbolic-ref", "-q", "HEAD"], &[], None)
        .await
        .map_err(|e| git_err(&e))?;
    if head_ref != format!("refs/heads/{}", wt.branch) {
        return Err(Why::NotClean(format!("HEAD is {head_ref}")));
    }
    if !git_ok(repo, &["diff-index", "--cached", "--quiet", "HEAD", "--"]).await {
        return Err(Why::NotClean("the index differs from the head".to_string()));
    }
    for put in puts {
        // exit 0: ignored; exit 1: not ignored; anything else is a refusal too. A tracked path is never reported as ignored.
        match git_run(
            repo,
            &[
                "-c",
                "core.excludesFile=/dev/null",
                "check-ignore",
                "-q",
                "--",
                put.path,
            ],
            &[],
            None,
        )
        .await
        {
            Ok(out) if out.status.code() == Some(1) => {}
            Ok(out) if out.status.code() == Some(0) => return Err(Why::Ignored),
            Ok(out) => return Err(Why::Failed(format!("git check-ignore: {}", out.status))),
            Err(e) => return Err(git_err(&e)),
        }
        let path = repo.join(put.path);
        match (&put.was, std::fs::symlink_metadata(&path)) {
            (None, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            (None, _) => return Err(Why::NotAFile),
            (Some(was), Ok(meta)) => {
                let on_disk = crate::direct::snapshot::read_regular_nofollow(&path);
                if on_disk.as_deref() != Some(was.as_bytes()) {
                    return Err(Why::NotClean(format!("{} differs from the head", put.path)));
                }
                let executable =
                    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
                if executable != (put.mode == "100755") {
                    return Err(Why::NotClean(format!(
                        "{} has another mode than the head",
                        put.path
                    )));
                }
            }
            (Some(_), Err(_)) => return Err(Why::NotAFile),
        }
    }
    Ok(())
}

/// A write that did not finish: whether the file was opened (and so may be created, truncated or partly written) or the open itself failed.
struct WriteError {
    opened: bool,
    why: String,
}

/// Writes the file: a new one with `O_EXCL` (which refuses a link at the last name), an extended one opened without following a link.
fn write_file(repo: &Path, put: &Put) -> Result<(), WriteError> {
    let path = repo.join(put.path);
    let mut options = OpenOptions::new();
    options
        .write(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    if put.was.is_some() {
        options.truncate(true);
    } else {
        options.create_new(true).mode(0o644);
    }
    let mut f = options.open(&path).map_err(|e| WriteError {
        opened: false,
        why: format!("{}: {e}", put.path),
    })?;
    f.write_all(put.text.as_bytes()).map_err(|e| WriteError {
        opened: true,
        why: format!("{}: {e}", put.path),
    })
}

/// Puts the files back: an extended file gets its text again, a new file is removed. Every file is tried; the first failure is reported.
fn undo_files(repo: &Path, touched: &[&Put]) -> Result<(), String> {
    let mut failed: Option<String> = None;
    for put in touched.iter().rev() {
        let path = repo.join(put.path);
        let done = match &put.was {
            Some(was) => OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
                .open(&path)
                .and_then(|mut f| f.write_all(was.as_bytes())),
            None => std::fs::remove_file(&path),
        };
        if let Err(e) = done {
            failed.get_or_insert(format!("{}: {e}", put.path));
        }
    }
    failed.map_or(Ok(()), |why| {
        Err(format!("DOX starter: the undo failed: {why}"))
    })
}

/// Puts the index back to the tree of the head the run started from (`read-tree` runs no clean filter); the checkout's files are not touched.
async fn undo_index(repo: &Path, origin: &str) -> Result<(), String> {
    git_text(repo, &["read-tree", origin], &[], None)
        .await
        .map(|_| ())
        .map_err(|e| format!("DOX starter: the undo failed: {e}"))
}

// ---- git ---------------------------------------------------------------------------------------------------------------------------

async fn git_run(
    repo: &Path,
    args: &[&str],
    vars: &[(&str, &str)],
    index: Option<&Path>,
) -> Result<std::process::Output, WsError> {
    workspace::git_plumbing(repo, args, vars, index).await
}

/// The trimmed text of a git call that must succeed.
async fn git_text(
    repo: &Path,
    args: &[&str],
    vars: &[(&str, &str)],
    index: Option<&Path>,
) -> Result<String, WsError> {
    let out = git_run(repo, args, vars, index).await?;
    if !out.status.success() {
        return Err(WsError::Git(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, WsError> {
    let out = git_run(repo, args, &[], None).await?;
    if !out.status.success() {
        return Err(WsError::Git(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Whether a git call exits 0.
async fn git_ok(repo: &Path, args: &[&str]) -> bool {
    git_run(repo, args, &[], None)
        .await
        .is_ok_and(|o| o.status.success())
}

/// The path of the run's private index, in the run's private data folder; it is removed as soon as the tree is written.
fn index_path(agent_data: &Path) -> PathBuf {
    agent_data.join("dox-starter.index")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starter_text() -> Starter {
        Starter {
            root: "<!-- DOX -->\n\n# DOX framework\n\n# Project\n\nReplace this paragraph.\n"
                .to_string(),
            notice: Some("# Third-party notices\n\nMIT, Agent Zero.\n".to_string()),
        }
    }

    fn file(text: &str) -> Tracked {
        Tracked::File {
            mode: "100644".to_string(),
            text: Some(text.to_string()),
        }
    }

    #[test]
    fn b766_a_root_is_read_by_its_lines_with_a_byte_order_mark_and_carriage_returns() {
        let s = starter_text();
        // the framework line with a carriage return still counts as the line
        let dox = file("# DOX framework\r\nrest\r\n");
        assert_eq!(
            decide(&s, "", &dox, &Tracked::Absent),
            Err(Why::HasFramework)
        );
        // a lifecycle root with a byte order mark and a carriage return is still a lifecycle root
        let life = file("\u{feff}# Development lifecycle\r\nsteps\r\n");
        let puts = decide(&s, "", &life, &Tracked::Absent).unwrap();
        assert_eq!(puts[0].path, "AGENTS.md");
        assert!(
            puts[0]
                .text
                .starts_with("\u{feff}# Development lifecycle\r\nsteps\r\n\n<!-- DOX -->")
        );
        assert_eq!(
            puts[0].was.as_deref(),
            Some("\u{feff}# Development lifecycle\r\nsteps\r\n")
        );
        // the line has to be the whole line
        let own = file("Our # DOX framework notes\n");
        assert_eq!(decide(&s, "", &own, &Tracked::Absent), Err(Why::OwnText));
    }

    #[test]
    fn b766_a_blank_root_counts_as_none_and_is_given_the_starter_alone() {
        let s = starter_text();
        for blank in ["", "\n", "  \n\t\n", "\u{feff}", "\u{feff}\r\n"] {
            let puts = decide(&s, "", &file(blank), &Tracked::Absent)
                .unwrap_or_else(|why| panic!("{blank:?}: {why:?}"));
            assert_eq!(
                puts.iter().map(|p| p.path).collect::<Vec<_>>(),
                ["AGENTS.md", "THIRD-PARTY-NOTICES.md"],
                "{blank:?}"
            );
            assert_eq!(
                puts[0].text, s.root,
                "{blank:?}: nothing of a blank file is kept"
            );
            assert_eq!(
                puts[0].was.as_deref(),
                Some(blank),
                "{blank:?}: the undo puts the blank file back"
            );
            assert_eq!(puts[0].mode, "100644");
        }
        // the mode of the tracked file is kept
        let exec = Tracked::File {
            mode: "100755".to_string(),
            text: Some("\n".to_string()),
        };
        assert_eq!(
            decide(&s, "", &exec, &Tracked::Absent).unwrap()[0].mode,
            "100755"
        );
        // a file with any text is not blank
        assert_eq!(
            decide(&s, "", &file(" x \n"), &Tracked::Absent),
            Err(Why::OwnText)
        );
    }

    #[test]
    fn b766_a_root_that_is_not_text_a_link_or_a_folder_is_left_alone() {
        let s = starter_text();
        let binary = Tracked::File {
            mode: "100644".to_string(),
            text: None,
        };
        assert_eq!(
            decide(&s, "", &binary, &Tracked::Absent),
            Err(Why::NotAFile)
        );
        assert_eq!(
            decide(&s, "", &Tracked::Other, &Tracked::Absent),
            Err(Why::NotAFile)
        );
    }

    #[test]
    fn b766_the_notice_goes_with_the_framework_text_and_below_a_notices_file_that_does_not_name_it()
    {
        let s = starter_text();
        let puts = decide(&s, "", &Tracked::Absent, &Tracked::Absent).unwrap();
        assert_eq!(
            puts.iter().map(|p| p.path).collect::<Vec<_>>(),
            ["AGENTS.md", "THIRD-PARTY-NOTICES.md"]
        );
        // a starter without the framework line is the user's own default: no notice
        let own = Starter {
            root: "Our default.\n".to_string(),
            notice: s.notice.clone(),
        };
        let puts = decide(&own, "", &Tracked::Absent, &Tracked::Absent).unwrap();
        assert_eq!(puts.len(), 1);
        // a notices file that is not text, a link or a folder is not touched, and the root is still written
        let puts = decide(&s, "", &Tracked::Absent, &Tracked::Other).unwrap();
        assert_eq!(puts.len(), 1);
        // an existing file that names DOX is kept as it is
        let puts = decide(&s, "", &Tracked::Absent, &file("DOX is used here\n")).unwrap();
        assert_eq!(puts.len(), 1);
        let puts = decide(&s, "", &Tracked::Absent, &file("Left pad, MIT\n")).unwrap();
        assert_eq!(
            puts[1].text,
            "Left pad, MIT\n\n# Third-party notices\n\nMIT, Agent Zero.\n"
        );
    }

    #[test]
    fn b766_the_off_switch_and_a_ban_come_before_the_root_is_read() {
        let off = Starter {
            root: "DOX off\n".to_string(),
            notice: None,
        };
        assert_eq!(
            decide(&off, "", &Tracked::Absent, &Tracked::Absent),
            Err(Why::Off)
        );
        assert_eq!(
            decide(
                &starter_text(),
                "Write no AGENTS.md docs.",
                &Tracked::Absent,
                &Tracked::Absent
            ),
            Err(Why::Banned)
        );
    }

    #[test]
    fn b766_text_is_added_below_text_with_one_blank_line() {
        assert_eq!(below("a\n", "b\n"), "a\n\nb\n");
        assert_eq!(below("a", "b\n"), "a\n\nb\n");
        assert_eq!(below("", "b\n"), "\n\nb\n");
    }
}

#[cfg(test)]
mod repo_tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DATE: &str = "1700000000 +0000";
    const BRANCH: &str = "harness/run/direct";

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", DATE)
            .env("GIT_COMMITTER_DATE", DATE)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Where these tests make their repositories: the folder `TMPDIR` names (every run of this project's tests sets it), else the working directory.
    fn scratch_root() -> PathBuf {
        std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("."), PathBuf::from)
    }

    /// A repository of one commit at a fixed time, checked out on the run's branch, and a private folder for agentd's own files.
    fn repo(label: &str, files: &[(&str, &str)]) -> (Worktree, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = scratch_root().join(format!(
            "dox-unit-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        for (path, text) in files {
            let file = repo.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "base"]);
        git_in(&repo, &["checkout", "-q", "-B", BRANCH]);
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        (
            Worktree {
                root: base,
                repo,
                branch: BRANCH.to_string(),
            },
            data,
        )
    }

    fn starter() -> Starter {
        Starter {
            root: "<!-- DOX -->\n\n# DOX framework\n\n# Project\n\nReplace this paragraph.\n"
                .to_string(),
            notice: Some("# Third-party notices\n\nMIT, Agent Zero.\n".to_string()),
        }
    }

    async fn puts_for(wt: &Worktree, origin: &str) -> Vec<Put> {
        tracked_and_decide(wt, &starter(), "", origin)
            .await
            .unwrap()
    }

    #[test]
    fn b766_the_commit_time_is_read_from_the_raw_commit() {
        let raw = b"tree abc\nparent def\nauthor A B <a@b> 100 +0200\ncommitter C D <c@d> 1700000000 -0700\n\nmessage 1 2 3\n";
        assert_eq!(committer_time(raw), Some(1_700_000_000));
        assert_eq!(committer_time(b"tree abc\n\nno committer\n"), None);
        // a line in the message that looks like the header is not read
        let tricky = b"tree abc\nauthor A <a@b> 1 +0000\ncommitter C <c@d> 2 +0000\n\ncommitter X <x@y> 3 +0000\n";
        assert_eq!(committer_time(tricky), Some(2));
    }

    #[tokio::test]
    async fn b766_the_commit_is_a_pure_function_of_its_inputs() {
        let (wt, data) = repo("pure", &[("README.md", "hello\n")]);
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        let puts = puts_for(&wt, &origin).await;
        let first = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        // a leftover file of a killed run does not stop the next one, and the id does not change
        std::fs::write(data.join("dox-starter-0.blob"), "left over").unwrap();
        std::fs::write(data.join("dox-starter.index"), "left over").unwrap();
        let second = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        assert_eq!(first, second);
        assert!(!data.join("dox-starter-0.blob").exists());
        assert!(!data.join("dox-starter.index").exists());
        // what the commit holds: the head's tree and the two files, by the harness, at the head's commit time
        assert_eq!(
            git_in(
                &wt.repo,
                &[
                    "log",
                    "-1",
                    "--format=%an|%ae|%cn|%ce|%at|%ct|%s|%P",
                    &first
                ]
            ),
            format!(
                "ham harness|harness@harness.invalid|ham harness|harness@harness.invalid|1700000000|1700000000|{SEED_SUBJECT}|{origin}"
            )
        );
        assert_eq!(
            git_in(&wt.repo, &["ls-tree", "-r", "--name-only", &first]),
            "AGENTS.md\nREADME.md\nTHIRD-PARTY-NOTICES.md"
        );
        // the id of fixed inputs is pinned: a change of the message, the identity or the encoding changes a run in flight
        assert_eq!(
            first, "024dfc9322ecc43b167a1a434ddf0a42edfea7d6",
            "the id of the commit for the pinned inputs"
        );
    }

    #[tokio::test]
    async fn b766_a_file_that_cannot_be_written_puts_the_earlier_one_back() {
        let (wt, data) = repo(
            "unwritable",
            &[("THIRD-PARTY-NOTICES.md", "Left pad, MIT.\n")],
        );
        let notices = wt.repo.join("THIRD-PARTY-NOTICES.md");
        let mut perms = std::fs::metadata(&notices).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
        std::fs::set_permissions(&notices, perms).unwrap();
        if OpenOptions::new().write(true).open(&notices).is_ok() {
            return; // running as root: a read-only file can be written, so the case cannot be made here
        }
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        let puts = puts_for(&wt, &origin).await;
        assert_eq!(
            puts.len(),
            2,
            "the trigger: the root is new, the notices file is extended"
        );
        let seed = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        let answer = apply(&wt, &origin, &seed, &puts).await.unwrap();
        assert!(matches!(answer, Err(Why::Failed(_))), "{answer:?}");
        assert!(
            !wt.repo.join("AGENTS.md").exists(),
            "the first file is gone again"
        );
        assert_eq!(
            std::fs::read_to_string(&notices).unwrap(),
            "Left pad, MIT.\n"
        );
        assert_eq!(git_in(&wt.repo, &["rev-parse", "HEAD"]), origin);
        assert_eq!(git_in(&wt.repo, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn b766_a_branch_that_moved_undoes_the_files_and_the_index_of_a_new_and_an_extended_file()
    {
        let (wt, data) = repo(
            "moved",
            &[
                ("AGENTS.md", "# Development lifecycle\nsteps\n"),
                ("THIRD-PARTY-NOTICES.md", "Left pad, MIT.\n"),
            ],
        );
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        // the branch moves on, to a commit of the same tree: the compare-and-swap on the head the commit was made for must fail
        git_in(&wt.repo, &["commit", "-q", "--allow-empty", "-m", "later"]);
        let moved = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        let puts = puts_for(&wt, &origin).await;
        assert!(
            puts.iter().all(|p| p.was.is_some()),
            "the trigger: both files are extended"
        );
        let seed = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        let answer = apply(&wt, &origin, &seed, &puts).await.unwrap();
        assert!(matches!(answer, Err(Why::Failed(_))), "{answer:?}");
        assert_eq!(git_in(&wt.repo, &["rev-parse", "HEAD"]), moved);
        assert_eq!(
            std::fs::read_to_string(wt.repo.join("AGENTS.md")).unwrap(),
            "# Development lifecycle\nsteps\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.repo.join("THIRD-PARTY-NOTICES.md")).unwrap(),
            "Left pad, MIT.\n"
        );
        assert_eq!(
            git_in(&wt.repo, &["status", "--porcelain"]),
            "",
            "the index agrees with the head again"
        );
    }

    #[tokio::test]
    async fn b766_a_success_leaves_the_tree_the_index_and_the_head_in_agreement() {
        let (wt, data) = repo("agree", &[("README.md", "hello\n")]);
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        let puts = puts_for(&wt, &origin).await;
        let seed = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        assert_eq!(apply(&wt, &origin, &seed, &puts).await.unwrap(), Ok(()));
        assert_eq!(git_in(&wt.repo, &["rev-parse", "HEAD"]), seed);
        assert_eq!(git_in(&wt.repo, &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(wt.repo.join("AGENTS.md")).unwrap(),
            starter().root
        );
    }

    #[tokio::test]
    async fn b766_no_program_the_repository_names_runs() {
        let (wt, data) = repo(
            "hostile",
            &[
                ("README.md", "hello\n"),
                (".gitattributes", "* filter=mark\n"),
            ],
        );
        // a clean filter that would leave a marker if git ever ran it
        let marker = wt.root.join("filter-used");
        let script = wt.root.join("filter.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho used > {}\ncat\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        git_in(
            &wt.repo,
            &["config", "filter.mark.clean", &script.display().to_string()],
        );
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        let puts = puts_for(&wt, &origin).await;
        let seed = build_commit(&wt, &data, &origin, &puts).await.unwrap();
        assert_eq!(apply(&wt, &origin, &seed, &puts).await.unwrap(), Ok(()));
        assert!(!marker.exists(), "the filter of the repository ran");
    }

    #[tokio::test]
    async fn b766_a_tracked_name_in_another_case_is_not_given_a_twin() {
        let (wt, _data) = repo("case", &[("agents.md", "mine\n")]);
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        assert_eq!(tracked(&wt, &origin, "AGENTS.md").await, Ok(Tracked::Other));
        assert_eq!(
            tracked_and_decide(&wt, &starter(), "", &origin).await,
            Err(Why::NotAFile)
        );
        // a tracked name of another file is no match
        assert_eq!(
            tracked(&wt, &origin, "THIRD-PARTY-NOTICES.md").await,
            Ok(Tracked::Absent)
        );
    }

    #[tokio::test]
    async fn b766_a_root_listing_reads_a_file_a_link_and_a_folder() {
        let (wt, _data) = repo(
            "kinds",
            &[("AGENTS.md", "own text\n"), ("sub/x.txt", "x\n")],
        );
        let origin = git_in(&wt.repo, &["rev-parse", "HEAD"]);
        assert_eq!(
            tracked(&wt, &origin, "AGENTS.md").await,
            Ok(Tracked::File {
                mode: "100644".to_string(),
                text: Some("own text\n".to_string())
            })
        );
        assert_eq!(tracked(&wt, &origin, "sub").await, Ok(Tracked::Other));
        assert_eq!(
            tracked(&wt, &origin, "missing.md").await,
            Ok(Tracked::Absent)
        );
    }
}
