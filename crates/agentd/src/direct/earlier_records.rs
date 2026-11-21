// COMPAT-CAH-DATA: this file names the earlier records folder, which the step handles in its own place until VERSION 6.00.
//! A follow-up of a run that reported done starts without the lifecycle records of the earlier request in the work tree.
//!
//! A follow-up starts on the commit the earlier run delivered (the seed, `continueFrom.headSha`). That tree holds the earlier request's goal
//! card, run log, red records and research notes at the live paths, and an engine that finds a card at the path the goal text names takes it
//! for this task's card: it edits the card's checks and then answers that they pass. So before the first prompt the harness makes one commit
//! of its own on the seed. The commit takes the earlier records out of the work tree and is the base of the follow-up's own work, as the DOX
//! starter commit is the base of a first request ([`crate::direct::dox`]). Nothing is moved into a folder and nothing is lost: the records are
//! in the parent of the commit, which is in the history the follow-up delivers, and in the earlier run's zip. The harness adds no check, no
//! gate and no revert on what the engine does afterwards, and it tells the engine nothing new: the subject and body of the commit are the one
//! text about the earlier run that the engine can read in the history.
//!
//! When the step runs. The stored request carries `continueFrom` with a head, and its `removeEarlierRecords` member is `true`
//! (`harness_protocol::direct::removes_earlier_records`). control-api is the only writer of `true`: it stores it for the start of a
//! follow-up of a run that reported done. A request without the member keeps the records, as the releases before the member did: a
//! resume and a follow-up of a run that did not report done, because there the card is the open task's own, and a request an older
//! service stored. `false` keeps them too: `ham` sends it for the user's `run.remove_earlier_records = false`.
//!
//! What goes. The entries of the seed's tree under `lifecycle/` and `research/` of the hidden records folder (`.ham`, or `.cah` when the seed
//! keeps its state there) that only runs of the product committed: every commit of the seed's history that touched the entry's path has an
//! author address and a committer address among the product's two identities ([`workspace::AGENT_AUTHOR`], the engine's commits and the
//! worker's end-of-turn commit; [`workspace::HARNESS_AUTHOR`], the harness's own commits, which include this commit of an earlier follow-up).
//! A file a person committed, or a person's file the engine later edited, stays. So does a person's file that the engine renamed inside
//! the two folders or moved into them: where a commit added the entry's path, git's rename and copy detection names the path it came
//! from in that commit's parent, and the history of that path counts too, and so on back. When that cannot be read with certainty (a
//! commit that changes too many files to tell a rename, a chain of renames longer than the step follows), the entry stays. `context/`,
//! the locator `records` and `test-command` stay. A records folder that the locator names (`.ham/records` = `docs`), records that are off
//! and an invalid locator are left alone.
//!
//! The history is read once for both folders, whatever the number of entries: one walk that lists the commits that changed something
//! below them, and one read of what each of those commits changed, grouped by path here. A rename that is followed costs one more read
//! of that kind for the path it came from. The line for agentd's log ends with the seconds the step took.
//!
//! Part one reads the seed commit only, never the work tree: the folder, the locator, the entries and their history come from the object
//! store. The commit is built with git plumbing (a private index, the entries taken out, a tree, a commit with the seed as its only parent,
//! the harness as author and committer, both dates the committer time of the seed), so the same seed gives the same commit in every session,
//! in a second invocation and in a retry.
//!
//! Part two looks at the checkout:
//!
//! * HEAD is the seed and this invocation just cloned the checkout: the commit is put on it (the files, the index and, last, the branch) and
//!   is the base. This is the first session, and a later session that restored the original bundle because nothing was saved.
//! * HEAD is not the seed: nothing is made. The base is the commit when it is HEAD or in its history (a later session that restored its
//!   predecessor's work; a second invocation after the commit was made), else the origin the caller names.
//! * HEAD is the seed in a checkout an earlier invocation left: nothing is made, as for the DOX starter, and the base is the origin.
//!
//! Best effort, as the DOX starter: a state in which the step cannot be made safely (a submodule below the records, more than
//! [`MAX_ENTRIES`] entries, an index that differs from the head, a file below the records that differs from the head or is untracked, a
//! refused git call) leaves the tree as it was, gives one line for agentd's log, and the run goes on. Only an undo that fails ends the run
//! before the prompt, because the work tree is then not known. The step never takes the last state out of `.cah`: the folder is chosen
//! again at the start of every session, and a `.cah` without state would make a later session of the same run choose `.ham`.

use crate::direct::dox::{self, Settled};
use crate::direct::records::{self, Records};
use crate::workspace::{self, Worktree, WsError};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The subject of the commit that takes the earlier request's records out of the work tree.
pub const SUBJECT: &str = "harness: earlier request's records removed from the work tree";

/// The message of the commit. It is a constant and names no folder: the commit is a function of the seed alone.
const MESSAGE: &str = "harness: earlier request's records removed from the work tree\n\n\
This run is a follow-up of a run that reported done. Before its first prompt the harness took the lifecycle records of the earlier request \
(the goal card, the run log, the red records and the research notes) out of the work tree, so that this request starts with records of its \
own. Nothing is lost: they are in the parent of this commit and in the zip of the earlier run. `git show <this commit>^:<path>` prints one. \
`run.remove_earlier_records = false` in ham's config.toml leaves them in place for your runs.\n";

/// The folders of the records folder whose entries go: the card, the log and the red records, and the research notes.
pub const FOLDERS: [&str; 2] = ["lifecycle", "research"];

/// The most entries under the two folders the step handles; with more nothing is done.
pub const MAX_ENTRIES: usize = 1000;

/// The limit git's rename detection is given (`-l`), as a fixed number so that no setting of the repository decides it: git's own default.
/// A commit that adds and removes more files than git compares under it cannot be read for renames, and what it added stays.
const RENAME_LIMIT: usize = 1000;

/// How far a chain of renames is followed back, and how many paths are followed in all; past either, the entry stays.
const MAX_RENAME_DEPTH: usize = 16;
const MAX_FOLLOWED: usize = 64;

/// Path names are taken literally by every history read: a name with a `*` or a `[` in it is that name.
const LITERAL: (&str, &str) = ("GIT_LITERAL_PATHSPECS", "1");

/// What makes a records folder hold state beside the two folders (`records::choose`): its locator, its command file and its context folder.
const STATE_FILES: [&str; 2] = ["records", "test-command"];
const CONTEXT: &str = "context";

/// The most bytes of a locator (`records.rs` has the same limit).
const MAX_LOCATOR_BYTES: u64 = 4096;

/// The addresses of the product's two identities: an entry goes only when no other address is in its history.
const PRODUCT: [&str; 2] = [workspace::AGENT_AUTHOR.1, workspace::HARNESS_AUTHOR.1];

/// How many paths one git call is given.
const CHUNK: usize = 100;

/// What [`settle`] needs from the invocation.
#[derive(Debug)]
pub struct Request<'a> {
    /// `continueFrom.headSha` of the stored request: the seed.
    pub head_sha: Option<&'a str>,
    /// `harness_protocol::direct::removes_earlier_records` of the stored request.
    pub remove: bool,
    /// The records folder the run chose from the checkout as restored (`records::choose`): `.ham` or `.cah`.
    pub folder: &'a str,
    /// The run's folder under agentd's private data directory (the private index is made there, never in the work tree).
    pub agent_data: &'a Path,
}

/// The base commit of the run's own work with the line for agentd's log, and the commit the step computes for the seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub settled: Settled,
    /// The commit the step computes for the seed, whether or not this invocation made it; `None` when it computes none. The `continue_from`
    /// tripwire accepts exactly this commit beside the seed.
    pub commit: Option<String>,
}

/// One entry of the seed's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    kind: String,
    id: String,
    path: String,
}

impl Entry {
    fn is_regular(&self) -> bool {
        self.kind == "blob" && matches!(self.mode.as_str(), "100644" | "100755")
    }
    fn is_link(&self) -> bool {
        self.kind == "blob" && self.mode == "120000"
    }
}

/// What part one computed from the seed commit alone.
#[derive(Debug)]
struct Plan {
    /// The records folder of the seed's tree.
    folder: &'static str,
    /// Every entry under the two folders.
    all: Vec<Entry>,
    /// The entries that go.
    remove: Vec<Entry>,
    /// The commit on the seed without them.
    commit: String,
}

fn kept(base: &str, why: &str, commit: Option<String>) -> Outcome {
    Outcome {
        settled: Settled {
            base: base.to_string(),
            note: Some(format!("not removed: {why}")),
        },
        commit,
    }
}

fn based(base: &str, note: String, commit: &str) -> Outcome {
    Outcome {
        settled: Settled {
            base: base.to_string(),
            note: Some(note),
        },
        commit: Some(commit.to_string()),
    }
}

fn git_why(e: &WsError) -> String {
    e.to_string()
}

/// The base commit of the run's work, after the commit that takes the earlier records out is made when it is due.
///
/// `origin` is the base the caller would use without this step: the head of the checkout in the first session, the run's original base the
/// service names in a later one (for a follow-up that is the seed). `fresh` is true when this invocation just cloned the checkout.
///
/// `Err` only when an undo failed, so the work tree is not known; the run does not start.
///
/// The line for the log ends with the seconds the step took (`; the step took 0.21 s`), so the cost of the step at the start of a run is
/// a number in every run's log.
pub async fn settle(
    wt: &Worktree,
    req: &Request<'_>,
    origin: &str,
    fresh: bool,
) -> Result<Outcome, String> {
    let began = std::time::Instant::now();
    let mut outcome = settle_untimed(wt, req, origin, fresh).await?;
    if let Some(note) = outcome.settled.note.as_mut() {
        note.push_str(&format!(
            "; the step took {:.2} s",
            began.elapsed().as_secs_f64()
        ));
    }
    Ok(outcome)
}

async fn settle_untimed(
    wt: &Worktree,
    req: &Request<'_>,
    origin: &str,
    fresh: bool,
) -> Result<Outcome, String> {
    let Some(seed) = req.head_sha else {
        return Ok(kept(
            origin,
            "the stored request names no seed commit",
            None,
        ));
    };
    if !req.remove {
        return Ok(kept(
            origin,
            "the stored request does not ask for it (removeEarlierRecords is not true)",
            None,
        ));
    }
    if let Err(e) = workspace::check_git_dir(&wt.repo) {
        return Ok(kept(origin, &git_why(&e), None));
    }
    let plan = match plan(wt, seed, req.agent_data).await {
        Ok(p) => p,
        Err(why) => return Ok(kept(origin, &why, None)),
    };
    let repo = wt.repo.as_path();
    let head = match dox::git_text(repo, &["rev-parse", "HEAD"], &[], None).await {
        Ok(h) => h,
        Err(e) => return Ok(kept(origin, &git_why(&e), Some(plan.commit))),
    };
    if head != seed {
        // Work on top of the seed: a later session, or a second invocation. The commit is in the history or it is not; nothing is made.
        let inside = head == plan.commit
            || dox::git_run(
                repo,
                &["merge-base", "--is-ancestor", &plan.commit, &head],
                &[],
                None,
            )
            .await
            .is_ok_and(|o| o.status.code() == Some(0));
        return Ok(if inside {
            based(
                &plan.commit,
                format!(
                    "the run's base is the commit {} that took the earlier records out",
                    dox::short(&plan.commit)
                ),
                &plan.commit,
            )
        } else {
            kept(
                origin,
                "the commit that takes the earlier records out is not in the restored history",
                Some(plan.commit),
            )
        });
    }
    if !fresh {
        return Ok(kept(
            origin,
            "the checkout was made by an earlier invocation",
            Some(plan.commit),
        ));
    }
    if req.folder != plan.folder {
        return Ok(kept(
            origin,
            &format!(
                "the run works in {} and the seed keeps its records in {}",
                req.folder, plan.folder
            ),
            Some(plan.commit),
        ));
    }
    match apply(wt, seed, &plan).await {
        Ok(Ok(())) => Ok(based(
            &plan.commit,
            format!(
                "removed {} of {} entries under {}/{} and {}/{}, commit {}",
                plan.remove.len(),
                plan.all.len(),
                plan.folder,
                FOLDERS[0],
                plan.folder,
                FOLDERS[1],
                dox::short(&plan.commit)
            ),
            &plan.commit,
        )),
        Ok(Err(why)) => Ok(kept(origin, &why, Some(plan.commit))),
        Err(fatal) => Err(fatal),
    }
}

// ---- part one: the seed commit --------------------------------------------------------------------------------------------------------

/// The entries of `listing` (`git ls-tree -r -z`): `<mode> <type> <id>\t<path>` each. `Err` for a name that is no UTF-8 text.
fn entries(listing: &[u8]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for raw in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(raw).map_err(|_| {
            "an entry below the records has a name that is no UTF-8 text".to_string()
        })?;
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| "git ls-tree: a line without a path".to_string())?;
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
            return Err("git ls-tree: a line without a mode, a type and an id".to_string());
        };
        out.push(Entry {
            mode: mode.to_string(),
            kind: kind.to_string(),
            id: id.to_string(),
            path: path.to_string(),
        });
    }
    Ok(out)
}

fn below(entry: &Entry, dir: &str) -> bool {
    entry
        .path
        .strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether the entries of the seed's tree make `base` hold state, read as `records::choose` reads a work tree: a regular file `records` or
/// `test-command` directly in it, or a regular file at any depth under `lifecycle/`, `research/` or `context/`. A link counts for nothing.
fn holds_state(tree: &[Entry], base: &str, without: &BTreeSet<&str>) -> bool {
    tree.iter()
        .filter(|e| e.is_regular() && !without.contains(e.path.as_str()))
        .any(|e| {
            STATE_FILES.iter().any(|f| e.path == format!("{base}/{f}"))
                || FOLDERS
                    .iter()
                    .chain(std::iter::once(&CONTEXT))
                    .any(|d| below(e, &format!("{base}/{d}")))
        })
}

/// What the locator of the seed's tree says, read as `Records::read_in` reads the file: no entry, or an entry that is no regular file, is no
/// locator. `Err` is the reason the step does not handle the layout.
async fn layout(repo: &Path, tree: &[Entry], folder: &str) -> Result<(), String> {
    let locator = format!("{folder}/records");
    let Some(entry) = tree.iter().find(|e| e.path == locator && e.is_regular()) else {
        return Ok(());
    };
    let size = dox::git_text(repo, &["cat-file", "-s", &entry.id], &[], None)
        .await
        .map_err(|e| git_why(&e))?;
    if !size.parse::<u64>().is_ok_and(|n| n <= MAX_LOCATOR_BYTES) {
        return Err(format!("the locator {locator} of the seed is invalid"));
    }
    let bytes = dox::git_bytes(repo, &["cat-file", "blob", &entry.id])
        .await
        .map_err(|e| git_why(&e))?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(format!("the locator {locator} of the seed is invalid"));
    };
    match Records::parse(&text) {
        Records::Default => Ok(()),
        Records::Moved(dir) if dir == folder => Ok(()),
        Records::Moved(dir) => Err(format!(
            "the layout is not handled: {locator} names the folder {dir}, and the harness leaves a folder of your own as it is"
        )),
        Records::Off => Err(format!("the records are switched off in {locator}")),
        Records::Invalid(_) | Records::Conflict(_) => {
            Err(format!("the locator {locator} of the seed is invalid"))
        }
    }
}

/// One commit of a history read ([`history`]): its parents, its two addresses, and what it changed at or below the paths that were asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Touch {
    id: String,
    parents: Vec<String>,
    author: String,
    committer: String,
    /// `(status, path)` against any parent: `A` added, `M` changed, `D` removed, `T` changed its type.
    changes: Vec<(char, String)>,
}

impl Touch {
    /// Whether a run of the product made the commit: the author address and the committer address are both among [`PRODUCT`].
    fn by_product(&self) -> bool {
        PRODUCT.contains(&self.author.as_str()) && PRODUCT.contains(&self.committer.as_str())
    }

    fn changed(&self, path: &str) -> bool {
        self.changes.iter().any(|(_, p)| p == path)
    }

    fn added(&self, path: &str) -> bool {
        self.changes.iter().any(|(s, p)| *s == 'A' && p == path)
    }
}

fn unreadable(what: &str) -> WsError {
    WsError::Git(format!("the history cannot be read with certainty: {what}"))
}

fn failed(what: &str, out: &std::process::Output) -> WsError {
    WsError::Git(format!(
        "git {what}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The commits of a walk (`git rev-list --format='P %P%nA %ae%nC %ce'`): `commit <id>` heads each record, and the three lines after it are
/// the format's. A record without all three is no record the step can use.
fn read_walk(text: &str) -> Result<Vec<Touch>, WsError> {
    let mut out: Vec<Touch> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(id) = line.strip_prefix("commit ") else {
            return Err(unreadable("a line of the walk is no commit"));
        };
        let (Some(p), Some(a), Some(c)) = (lines.next(), lines.next(), lines.next()) else {
            return Err(unreadable("a commit of the walk is cut short"));
        };
        // A commit without a parent prints `P ` and git may trim the blank.
        let (Some(parents), Some(author), Some(committer)) = (
            p.strip_prefix('P'),
            a.strip_prefix("A "),
            c.strip_prefix("C "),
        ) else {
            return Err(unreadable("a commit of the walk lacks a line"));
        };
        out.push(Touch {
            id: id.to_string(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            author: author.to_string(),
            committer: committer.to_string(),
            changes: Vec::new(),
        });
    }
    Ok(out)
}

/// Lays what each commit changed into `touches`. `listing` is the output of `git diff-tree --stdin -r -m --root -z --no-renames
/// --name-status`: the id of a commit, then `<status>`, `<path>` pairs, each ended by a NUL; a merge has one such block per parent that
/// differs, and every block counts for the commit.
fn read_changes(listing: &[u8], touches: &mut [Touch]) -> Result<(), WsError> {
    let at: BTreeMap<String, usize> = touches
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.clone(), i))
        .collect();
    let mut current: Option<usize> = None;
    let mut tokens = listing.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        let text = std::str::from_utf8(token)
            .map_err(|_| unreadable("a name in the history is no UTF-8 text"))?;
        if let Some(i) = at.get(text) {
            current = Some(*i);
            continue;
        }
        let mut letters = text.chars();
        let (Some(status), None) = (letters.next(), letters.next()) else {
            return Err(unreadable("an entry of a commit has no status"));
        };
        let path = tokens
            .next()
            .ok_or_else(|| unreadable("an entry of a commit has no path"))?;
        let path = std::str::from_utf8(path)
            .map_err(|_| unreadable("a name in the history is no UTF-8 text"))?;
        let Some(i) = current else {
            return Err(unreadable("an entry comes before its commit"));
        };
        if !touches[i].changed(path) || status == 'A' {
            touches[i].changes.push((status, path.to_string()));
        }
    }
    Ok(())
}

/// The commits of the history of `start` that changed something at or below `paths`, each with what it changed there. Two git calls
/// whatever the number of entries: one walk of the history, in full (a merge is not simplified away) and with the paths taken literally,
/// and one read of what each of those commits changed against its parents. Both are plumbing: no setting of the repository for `git log`
/// or `git diff` applies to them.
async fn history(
    repo: &Path,
    agent_data: &Path,
    start: &str,
    paths: &[&str],
) -> Result<Vec<Touch>, WsError> {
    let mut args = vec![
        "rev-list",
        "--full-history",
        "--format=P %P%nA %ae%nC %ce",
        start,
        "--",
    ];
    args.extend(paths);
    let out = dox::git_run(repo, &args, &[LITERAL], None).await?;
    if !out.status.success() {
        return Err(failed("rev-list", &out));
    }
    let mut touches = read_walk(&String::from_utf8_lossy(&out.stdout))?;
    if touches.is_empty() {
        return Ok(touches);
    }
    // The list of those commits is the input of the second call: a private file beside the private index, gone as soon as it was read.
    let list = agent_data.join("earlier-records.commits");
    let ids: String = touches.iter().map(|t| format!("{}\n", t.id)).collect();
    std::fs::write(&list, ids)
        .map_err(|e| WsError::Git(format!("the list of commits cannot be written: {e}")))?;
    let mut args = vec![
        "diff-tree",
        "--stdin",
        "-r",
        "-m",
        "--root",
        "-z",
        "--no-renames",
        "--name-status",
        "--",
    ];
    args.extend(paths);
    let out = workspace::git_plumbing_fed(repo, &args, &[LITERAL], &list).await;
    let _ = std::fs::remove_file(&list);
    let out = out?;
    if !out.status.success() {
        return Err(failed("diff-tree", &out));
    }
    read_changes(&out.stdout, &mut touches)?;
    Ok(touches)
}

/// The `(from, to)` pairs of the renames and copies in the output of `git diff-tree -r -z -M -C --name-status`: an `R<score>` or
/// `C<score>` status is followed by two paths, every other status by one.
fn read_renames(listing: &[u8]) -> Result<Vec<(String, String)>, WsError> {
    let text = |t: &[u8]| {
        std::str::from_utf8(t)
            .map(str::to_string)
            .map_err(|_| unreadable("a name in the history is no UTF-8 text"))
    };
    let mut out = Vec::new();
    let mut tokens = listing.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(status) = tokens.next() {
        let first = tokens
            .next()
            .ok_or_else(|| unreadable("an entry of a commit has no path"))?;
        if matches!(status.first(), Some(b'R' | b'C')) {
            let second = tokens
                .next()
                .ok_or_else(|| unreadable("a rename has no second path"))?;
            out.push((text(first)?, text(second)?));
        }
    }
    Ok(out)
}

/// The `(from, to)` pairs of the renames and copies of one commit against one parent; `None` when git could not tell them.
type Renames = Option<Vec<(String, String)>>;

/// Reads who committed the entries, following a rename or a move back to the path the content came from.
struct Owners<'a> {
    repo: &'a Path,
    agent_data: &'a Path,
    /// The renames and copies of one commit against one parent, read once: `None` when git could not tell them.
    renames: BTreeMap<(String, String), Renames>,
    /// How many paths were followed back so far.
    followed: usize,
}

impl Owners<'_> {
    /// The paths `path` was renamed or copied from in `commit`, against its parent `parent`; `None` when the commit changes too many
    /// files for git to compare them all, so that a rename could be there unseen.
    async fn sources(
        &mut self,
        parent: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<Vec<String>>, WsError> {
        let key = (parent.to_string(), commit.to_string());
        let pairs = match self.renames.get(&key) {
            Some(known) => known.clone(),
            None => {
                let read = self.read_renames_of(parent, commit).await?;
                self.renames.insert(key, read.clone());
                read
            }
        };
        Ok(pairs.map(|pairs| {
            pairs
                .into_iter()
                .filter(|(_, to)| to == path)
                .map(|(from, _)| from)
                .collect()
        }))
    }

    async fn read_renames_of(&self, parent: &str, commit: &str) -> Result<Renames, WsError> {
        // What git would have to compare: every added file with every removed or changed one. Past its limit it compares none of them
        // and says nothing, so the count is taken here first.
        let plain = dox::git_bytes(
            self.repo,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-status",
                parent,
                commit,
            ],
        )
        .await?;
        let (mut added, mut sources) = (0usize, 0usize);
        let mut tokens = plain.split(|b| *b == 0).filter(|t| !t.is_empty());
        while let Some(status) = tokens.next() {
            let _path = tokens.next();
            match status.first() {
                Some(b'A') => added += 1,
                Some(b'D' | b'M' | b'T') => sources += 1,
                _ => {}
            }
        }
        if added.saturating_mul(sources) > RENAME_LIMIT * RENAME_LIMIT {
            return Ok(None);
        }
        let limit = format!("-l{RENAME_LIMIT}");
        let found = dox::git_bytes(
            self.repo,
            &[
                "diff-tree",
                "-r",
                "-z",
                "-M",
                "-C",
                &limit,
                "--name-status",
                parent,
                commit,
            ],
        )
        .await?;
        read_renames(&found).map(Some)
    }

    /// Whether only runs of the product committed the entry at `path`: every commit of `walk` (the history of the two folders) that
    /// changed the path has an author address and a committer address among [`PRODUCT`], at least one commit did, and the same holds
    /// for every path the content came from by a rename or a copy, and so on back. `Ok(false)` also when that cannot be read with
    /// certainty: the entry then stays, which is what the releases before did with it.
    async fn product_only(&mut self, walk: &[Touch], path: &str) -> Result<bool, WsError> {
        // (the commits that changed the path, the path, how many renames back)
        let own: Vec<Touch> = walk.iter().filter(|t| t.changed(path)).cloned().collect();
        let mut open: Vec<(Vec<Touch>, String, usize)> = vec![(own, path.to_string(), 0)];
        while let Some((touches, path, depth)) = open.pop() {
            if touches.is_empty() || !touches.iter().all(Touch::by_product) {
                return Ok(false);
            }
            for touch in touches.iter().filter(|t| t.added(&path)) {
                for parent in &touch.parents {
                    let Some(sources) = self.sources(parent, &touch.id, &path).await? else {
                        return Ok(false);
                    };
                    for source in sources {
                        if depth >= MAX_RENAME_DEPTH || self.followed >= MAX_FOLLOWED {
                            return Ok(false);
                        }
                        self.followed += 1;
                        let earlier: Vec<Touch> =
                            history(self.repo, self.agent_data, parent, &[source.as_str()])
                                .await?
                                .into_iter()
                                .filter(|t| t.changed(&source))
                                .collect();
                        open.push((earlier, source, depth + 1));
                    }
                }
            }
        }
        Ok(true)
    }
}

/// Part one: what the seed commit alone says. `Err` is the reason nothing is removed, for agentd's log.
async fn plan(wt: &Worktree, seed: &str, agent_data: &Path) -> Result<Plan, String> {
    let repo = wt.repo.as_path();
    let present = format!("{seed}^{{commit}}");
    if !dox::git_ok(repo, &["cat-file", "-e", &present]).await {
        return Err("the seed commit is not in the restored repository".to_string());
    }
    // The state of the two hidden folders in the seed's tree, in one listing.
    let mut args: Vec<String> = ["ls-tree", "-r", "-z", seed, "--"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for base in [records::DEFAULT_BASE, records::EARLIER_BASE] {
        for name in STATE_FILES
            .iter()
            .chain(FOLDERS.iter())
            .chain(std::iter::once(&CONTEXT))
        {
            args.push(format!("{base}/{name}"));
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let listing = dox::git_bytes(repo, &arg_refs)
        .await
        .map_err(|e| git_why(&e))?;
    let tree = entries(&listing)?;
    let none = BTreeSet::new();
    let folder = match (
        holds_state(&tree, records::DEFAULT_BASE, &none),
        holds_state(&tree, records::EARLIER_BASE, &none),
    ) {
        (true, true) => {
            return Err(format!(
                "the seed keeps records in both {} and {}",
                records::DEFAULT_BASE,
                records::EARLIER_BASE
            ));
        }
        (false, true) => records::EARLIER_BASE,
        _ => records::DEFAULT_BASE,
    };
    layout(repo, &tree, folder).await?;
    let all: Vec<Entry> = tree
        .iter()
        .filter(|e| FOLDERS.iter().any(|d| below(e, &format!("{folder}/{d}"))))
        .cloned()
        .collect();
    if all.is_empty() {
        return Err("the seed holds no lifecycle records".to_string());
    }
    if all.len() > MAX_ENTRIES {
        return Err(format!(
            "more than {MAX_ENTRIES} entries are below the records of the seed"
        ));
    }
    if all.iter().any(|e| !e.is_regular() && !e.is_link()) {
        return Err("a submodule is below the records of the seed".to_string());
    }
    // Who committed the entries: the history of both folders in one read, grouped by path.
    let folders = [
        format!("{folder}/{}", FOLDERS[0]),
        format!("{folder}/{}", FOLDERS[1]),
    ];
    let walk = history(
        repo,
        agent_data,
        seed,
        &[folders[0].as_str(), folders[1].as_str()],
    )
    .await
    .map_err(|e| git_why(&e))?;
    let mut owners = Owners {
        repo,
        agent_data,
        renames: BTreeMap::new(),
        followed: 0,
    };
    let mut remove = Vec::new();
    for entry in &all {
        if owners
            .product_only(&walk, &entry.path)
            .await
            .map_err(|e| git_why(&e))?
        {
            remove.push(entry.clone());
        }
    }
    if remove.is_empty() {
        return Err("no record of the seed was committed by runs of the product alone".to_string());
    }
    if folder == records::EARLIER_BASE {
        let gone: BTreeSet<&str> = remove.iter().map(|e| e.path.as_str()).collect();
        if !holds_state(&tree, folder, &gone) {
            return Err(format!(
                "the step would take the last state out of {folder}, and a later session would then choose another records folder"
            ));
        }
    }
    let commit = build_commit(repo, agent_data, seed, &remove)
        .await
        .map_err(|e| git_why(&e))?;
    Ok(Plan {
        folder,
        all,
        remove,
        commit,
    })
}

/// Builds the commit with plumbing only: no file of the work tree is read, no hook, no attribute and no setting of the repository is
/// consulted. The result is a function of the seed and of the entries that go.
async fn build_commit(
    repo: &Path,
    agent_data: &Path,
    seed: &str,
    remove: &[Entry],
) -> Result<String, WsError> {
    let index = dox::index_path(agent_data);
    let result = build_commit_in(repo, &index, seed, remove).await;
    let _ = std::fs::remove_file(&index);
    result
}

async fn build_commit_in(
    repo: &Path,
    index: &Path,
    seed: &str,
    remove: &[Entry],
) -> Result<String, WsError> {
    let _ = std::fs::remove_file(index);
    dox::git_text(repo, &["read-tree", seed], &[], Some(index)).await?;
    for chunk in remove.chunks(CHUNK) {
        let mut args = vec!["update-index", "--force-remove", "--"];
        args.extend(chunk.iter().map(|e| e.path.as_str()));
        dox::git_text(repo, &args, &[], Some(index)).await?;
    }
    let tree = dox::git_text(repo, &["write-tree"], &[], Some(index)).await?;
    // Both dates are the commit time of the seed, read from the raw object: a fixed value, so the same seed gives the same commit.
    let raw = dox::git_bytes(repo, &["cat-file", "commit", seed]).await?;
    let when = dox::committer_time(&raw)
        .ok_or_else(|| WsError::Git("cat-file: the seed has no committer time".to_string()))?;
    let date = format!("{when} +0000");
    dox::git_text(
        repo,
        &[
            "-c",
            "i18n.commitEncoding=UTF-8",
            "commit-tree",
            &tree,
            "-p",
            seed,
            "-m",
            MESSAGE,
        ],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
        None,
    )
    .await
}

// ---- part two: the checkout -----------------------------------------------------------------------------------------------------------

/// Every path that is no folder below `dir` of the checkout, relative to the repository; a link is listed and never followed. `None` when
/// more than `budget` paths are there or a folder cannot be read.
fn files_below(
    repo: &Path,
    dir: &str,
    budget: &mut usize,
    out: &mut BTreeSet<String>,
) -> Option<()> {
    let path = repo.join(dir);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(()),
        Err(_) => return None,
    };
    if !meta.file_type().is_dir() {
        out.insert(dir.to_string());
        return Some(());
    }
    for entry in std::fs::read_dir(&path).ok()? {
        let entry = entry.ok()?;
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let name = entry.file_name().into_string().ok()?;
        let rel = format!("{dir}/{name}");
        if entry.file_type().ok()?.is_dir() {
            files_below(repo, &rel, budget, out)?;
        } else {
            out.insert(rel);
        }
    }
    Some(())
}

/// The checkout is as the bundle left it below the two folders: every entry of the head is there with the head's content and mode, and
/// nothing else is (an untracked or ignored file would be left behind, or lost).
async fn preconditions(wt: &Worktree, plan: &Plan) -> Result<(), String> {
    let repo = wt.repo.as_path();
    dox::as_cloned(wt).await.map_err(|why| why.words())?;
    let mut on_disk = BTreeSet::new();
    let mut budget = MAX_ENTRIES * 2;
    for dir in FOLDERS {
        files_below(
            repo,
            &format!("{}/{dir}", plan.folder),
            &mut budget,
            &mut on_disk,
        )
        .ok_or_else(|| {
            "the checkout is not as the bundle left it: the records folders cannot be listed"
                .to_string()
        })?;
    }
    let tracked: BTreeSet<String> = plan.all.iter().map(|e| e.path.clone()).collect();
    if let Some(extra) = on_disk.difference(&tracked).next() {
        return Err(format!(
            "the checkout is not as the bundle left it: {extra} is not a file of the head"
        ));
    }
    if let Some(missing) = tracked.difference(&on_disk).next() {
        return Err(format!(
            "the checkout is not as the bundle left it: {missing} is missing"
        ));
    }
    let differs = |path: &str| {
        format!("the checkout is not as the bundle left it: {path} differs from the head")
    };
    // The links: the target is the blob.
    for entry in plan.all.iter().filter(|e| e.is_link()) {
        let target =
            std::fs::read_link(repo.join(&entry.path)).map_err(|_| differs(&entry.path))?;
        let blob = dox::git_bytes(repo, &["cat-file", "blob", &entry.id])
            .await
            .map_err(|e| git_why(&e))?;
        if std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str()) != blob.as_slice() {
            return Err(differs(&entry.path));
        }
    }
    // The regular files: a file, no link, with the mode and the content of the head (hashed without a filter, as the blob was written).
    let regular: Vec<&Entry> = plan.all.iter().filter(|e| e.is_regular()).collect();
    for entry in &regular {
        let meta =
            std::fs::symlink_metadata(repo.join(&entry.path)).map_err(|_| differs(&entry.path))?;
        let executable = meta.permissions().mode() & 0o111 != 0;
        if !meta.file_type().is_file() || executable != (entry.mode == "100755") {
            return Err(differs(&entry.path));
        }
    }
    for chunk in regular.chunks(CHUNK) {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(chunk.iter().map(|e| e.path.as_str()));
        let ids = dox::git_text(repo, &args, &[], None)
            .await
            .map_err(|e| git_why(&e))?;
        let ids: Vec<&str> = ids.lines().collect();
        if ids.len() != chunk.len() {
            return Err("git hash-object: another number of ids than files".to_string());
        }
        for (entry, id) in chunk.iter().zip(ids) {
            if entry.id != id {
                return Err(differs(&entry.path));
            }
        }
    }
    Ok(())
}

/// The folders of `paths` up to and with `folder`, the deepest first.
fn folders_of(paths: &[&str], folder: &str) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for path in paths {
        let mut at = Path::new(path).parent();
        while let Some(dir) = at {
            let Some(text) = dir.to_str().filter(|t| !t.is_empty()) else {
                break;
            };
            dirs.insert(text.to_string());
            if text == folder {
                break;
            }
            at = dir.parent();
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    dirs
}

/// Puts the commit on the checkout: the files leave the work tree, then the folders that became empty, then their entries leave the
/// index and, last, the branch moves. `Ok(Err(why))` is a refusal with the tree as it was; `Err` is an undo that failed. It runs only in a checkout this
/// invocation just cloned, so the repository's own settings are the clone's.
async fn apply(wt: &Worktree, seed: &str, plan: &Plan) -> Result<Result<(), String>, String> {
    let repo = wt.repo.as_path();
    if let Err(why) = preconditions(wt, plan).await {
        return Ok(Err(why));
    }
    // 1. the files, each without following a link: `remove_file` takes a link away and leaves its target alone
    let mut gone: Vec<&Entry> = Vec::new();
    for entry in &plan.remove {
        let path = repo.join(&entry.path);
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_dir() => std::fs::remove_file(&path),
            Ok(_) => Err(std::io::Error::other("a folder is at the path of a file")),
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => gone.push(entry),
            Err(e) => {
                let why = format!("{}: {e}", entry.path);
                return undone(repo, seed, &gone, why).await;
            }
        }
    }
    // 2. the folders that became empty, the deepest first; a folder that still holds something stays (`remove_dir` refuses it)
    let paths: Vec<&str> = plan.remove.iter().map(|e| e.path.as_str()).collect();
    for dir in folders_of(&paths, plan.folder) {
        let _ = std::fs::remove_dir(repo.join(dir));
    }
    // 3. the index of the checkout: the entries of the files that left are taken out. The index equalled the seed's tree before, so it
    //    now equals the tree of the commit, and every other entry keeps what git knows about its file: the next `git status` does not
    //    read every tracked file of the repository again. No filter runs.
    for chunk in plan.remove.chunks(CHUNK) {
        let mut args = vec!["update-index", "--force-remove", "--"];
        args.extend(chunk.iter().map(|e| e.path.as_str()));
        if let Err(e) = dox::git_text(repo, &args, &[], None).await {
            return undone(repo, seed, &gone, git_why(&e)).await;
        }
    }
    // 4. the branch, last: a compare-and-swap on the seed, so a checkout whose HEAD is the commit has the files removed and the index read
    let branch = format!("refs/heads/{}", wt.branch);
    if let Err(e) = dox::git_text(
        repo,
        &["update-ref", "-m", SUBJECT, &branch, &plan.commit, seed],
        &[],
        None,
    )
    .await
    {
        return undone(repo, seed, &gone, git_why(&e)).await;
    }
    Ok(Ok(()))
}

/// Both undos run whatever the first one says; a failure of either is the fatal answer, because the tree is then not known.
async fn undone(
    repo: &Path,
    seed: &str,
    gone: &[&Entry],
    why: String,
) -> Result<Result<(), String>, String> {
    let index = dox::git_text(repo, &["read-tree", seed], &[], None)
        .await
        .map(|_| ())
        .map_err(|e| format!("earlier records: the undo failed: {e}"));
    let files = undo_files(repo, gone).await;
    match (index, files) {
        (Ok(()), Ok(())) => Ok(Err(format!("tree put back: {why}"))),
        (a, b) => Err([a.err(), b.err()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")),
    }
}

/// Writes the removed files back from their blobs, with the mode of the head. Every file is tried; the first failure is reported.
async fn undo_files(repo: &Path, gone: &[&Entry]) -> Result<(), String> {
    let mut failed: Option<String> = None;
    for entry in gone.iter().rev() {
        if let Err(e) = put_back(repo, entry).await {
            failed.get_or_insert(format!("{}: {e}", entry.path));
        }
    }
    failed.map_or(Ok(()), |why| {
        Err(format!("earlier records: the undo failed: {why}"))
    })
}

async fn put_back(repo: &Path, entry: &Entry) -> Result<(), String> {
    use std::io::Write as _;
    let path: PathBuf = repo.join(&entry.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let blob = dox::git_bytes(repo, &["cat-file", "blob", &entry.id])
        .await
        .map_err(|e| e.to_string())?;
    if entry.is_link() {
        let target = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&blob);
        return std::os::unix::fs::symlink(target, &path).map_err(|e| e.to_string());
    }
    let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(&blob).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn entry(mode: &str, path: &str) -> Entry {
        Entry {
            mode: mode.to_string(),
            kind: if mode == "160000" { "commit" } else { "blob" }.to_string(),
            id: "0".repeat(40),
            path: path.to_string(),
        }
    }

    #[test]
    fn b866_the_subject_is_the_first_line_of_the_message_and_the_body_names_no_folder() {
        assert_eq!(MESSAGE.lines().next(), Some(SUBJECT));
        assert!(MESSAGE.ends_with('\n'));
        for word in [".ham", ".cah", "lifecycle/", "research/"] {
            assert!(!MESSAGE.contains(word), "{word}");
        }
        assert!(MESSAGE.contains("run.remove_earlier_records = false"));
        assert!(MESSAGE.contains("git show <this commit>^:<path>"));
    }

    #[test]
    fn b866_the_product_addresses_are_the_two_identities_of_the_worker() {
        assert_eq!(
            PRODUCT,
            ["agent@harness.invalid", "harness@harness.invalid"]
        );
    }

    #[test]
    fn b866_a_listing_is_read_entry_by_entry() {
        let listing = b"100644 blob 1111111111111111111111111111111111111111\t.ham/lifecycle/GOAL.md\x00120000 blob 2222222222222222222222222222222222222222\t.ham/lifecycle/red/a b\x00";
        let got = entries(listing).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, ".ham/lifecycle/GOAL.md");
        assert!(got[0].is_regular() && !got[0].is_link());
        assert_eq!(got[1].path, ".ham/lifecycle/red/a b");
        assert!(got[1].is_link());
        assert!(entries(b"100644 blob 1111\t\xff\xfe\x00").is_err());
    }

    #[test]
    fn b866_state_is_read_from_a_tree_as_the_folder_choice_reads_a_work_tree() {
        let none = BTreeSet::new();
        let state = |paths: &[(&str, &str)], base: &str| {
            let tree: Vec<Entry> = paths.iter().map(|(m, p)| entry(m, p)).collect();
            holds_state(&tree, base, &none)
        };
        assert!(state(&[("100644", ".ham/records")], ".ham"));
        assert!(state(&[("100755", ".ham/test-command")], ".ham"));
        assert!(state(&[("100644", ".cah/context/INDEX.md")], ".cah"));
        assert!(state(&[("100644", ".ham/research/a/b/c.md")], ".ham"));
        // a link counts for nothing, and neither does a file beside the folders
        assert!(!state(&[("120000", ".ham/records")], ".ham"));
        assert!(!state(&[("100644", ".ham/notes.md")], ".ham"));
        assert!(!state(&[("100644", ".ham/lifecyclex/GOAL.md")], ".ham"));
        assert!(!state(&[("100644", ".ham/records")], ".cah"));
        // without the entries that go
        let tree = vec![entry("100644", ".cah/lifecycle/GOAL.md")];
        let gone: BTreeSet<&str> = [".cah/lifecycle/GOAL.md"].into_iter().collect();
        assert!(holds_state(&tree, ".cah", &none));
        assert!(!holds_state(&tree, ".cah", &gone));
    }

    const C1: &str = "da19fd33b76c1c3e7a71a40bdd4633f88d921e66";
    const C2: &str = "25489065f73a2f742e14bb0109dc6b5f150ab8a3";
    const C3: &str = "e5035af04ea16bcb7e1dfaedeb674e3ecafe4e89";

    #[test]
    fn b866_a_walk_is_read_commit_by_commit() {
        // As git prints it: a merge of a person, a commit of the engine, and a first commit, which has no parent.
        let text = format!(
            "commit {C1}\nP {C2} 140eaf7aa7a58fc8d63f15c989f3d99570692126\nA pat@example.com\nC pat@example.com\n\
             commit {C2}\nP {C3}\nA agent@harness.invalid\nC agent@harness.invalid\n\
             commit {C3}\nP \nA agent@harness.invalid\nC harness@harness.invalid\n"
        );
        let walk = read_walk(&text).unwrap();
        assert_eq!(walk.len(), 3);
        assert_eq!(walk[0].parents.len(), 2);
        assert!(!walk[0].by_product());
        assert_eq!(walk[1].parents, [C3]);
        assert!(walk[1].by_product());
        assert!(walk[2].parents.is_empty());
        assert!(walk[2].by_product(), "both identities of the worker count");
        assert!(read_walk("").unwrap().is_empty());
        // A walk that is cut short, or that holds a line of another shape, is not read as a shorter history.
        assert!(read_walk(&format!("commit {C1}\nP {C2}\nA pat@example.com\n")).is_err());
        assert!(read_walk("A pat@example.com\n").is_err());
        assert!(read_walk(&format!("commit {C1}\nA a@b\nC a@b\nP {C2}\n")).is_err());
    }

    #[test]
    fn b866_what_the_commits_changed_is_grouped_by_path() {
        let mut walk = read_walk(&format!(
            "commit {C1}\nP {C2} {C3}\nA pat@example.com\nC pat@example.com\n\
             commit {C2}\nP {C3}\nA agent@harness.invalid\nC agent@harness.invalid\n"
        ))
        .unwrap();
        // The merge has one block per parent that differs; both count for it.
        let listing = format!(
            "{C1}\0A\0.ham/lifecycle/SIDE.md\0{C1}\0D\0.ham/research/01-notes.md\0A\0.ham/research/03 again.md\0\
             {C2}\0M\0.ham/lifecycle/GOAL.md\0A\0.ham/research/03 again.md\0"
        );
        read_changes(listing.as_bytes(), &mut walk).unwrap();
        assert!(walk[0].added(".ham/lifecycle/SIDE.md"));
        assert!(walk[0].changed(".ham/research/01-notes.md"));
        assert!(!walk[0].added(".ham/research/01-notes.md"));
        assert!(walk[0].added(".ham/research/03 again.md"));
        assert!(
            walk[1].changed(".ham/lifecycle/GOAL.md") && !walk[1].added(".ham/lifecycle/GOAL.md")
        );
        // Grouped by path: who touched the note with a blank in its name.
        let touched: Vec<&str> = walk
            .iter()
            .filter(|t| t.changed(".ham/research/03 again.md"))
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(touched, [C1, C2]);
        assert!(!walk.iter().any(|t| t.changed(".ham/research/03")));
        // An entry before any commit, an entry without a path and a name that is no text are not read as an empty history.
        let mut walk = read_walk(&format!("commit {C1}\nP \nA a@b\nC a@b\n")).unwrap();
        assert!(read_changes(b"A\0.ham/lifecycle/GOAL.md\0", &mut walk).is_err());
        assert!(read_changes(format!("{C1}\0A\0").as_bytes(), &mut walk).is_err());
        let mut bad = format!("{C1}\0A\0").into_bytes();
        bad.extend([0xff, 0xfe, 0]);
        assert!(read_changes(&bad, &mut walk).is_err());
    }

    #[test]
    fn b866_renames_and_copies_are_read_with_both_paths() {
        let listing = b"R100\0docs/notes.md\0.ham/research/01-notes.md\0M\0README.md\0C075\0docs/a b.md\0.ham/research/02.md\0D\0old.txt\0";
        assert_eq!(
            read_renames(listing).unwrap(),
            [
                (
                    "docs/notes.md".to_string(),
                    ".ham/research/01-notes.md".to_string()
                ),
                ("docs/a b.md".to_string(), ".ham/research/02.md".to_string()),
            ]
        );
        assert!(read_renames(b"").unwrap().is_empty());
        assert!(read_renames(b"R100\0only-one\0").is_err());
        assert!(read_renames(b"A\0").is_err());
    }

    #[test]
    fn b866_the_folders_that_may_have_become_empty_are_listed_deepest_first() {
        let dirs = folders_of(
            &[
                ".ham/lifecycle/red/01-x.txt",
                ".ham/lifecycle/GOAL.md",
                ".ham/research/01-x.md",
            ],
            ".ham",
        );
        assert_eq!(
            dirs,
            [
                ".ham/lifecycle/red",
                ".ham/lifecycle",
                ".ham/research",
                ".ham"
            ]
        );
    }
}
