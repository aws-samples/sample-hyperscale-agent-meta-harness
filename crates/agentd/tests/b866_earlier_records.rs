// COMPAT-CAH-DATA: this file names the earlier records folder, which the step handles in its own place until VERSION 6.00.
//! A follow-up of a run that reported done starts without the lifecycle records of the earlier request in the work tree
//! (`src/direct/earlier_records.rs`, step 4 of `start_and_prompt`, and the `continue_from` tripwire).
//!
//! A follow-up starts on the commit the earlier run delivered (the seed). An engine that finds the earlier request's goal card at the
//! path the goal text names takes it for this task's card. So before the first prompt the harness makes one commit of its own on the
//! seed that takes the records runs of the product wrote under `lifecycle/` and `research/` of the hidden records folder out of the
//! work tree. They stay in the parent of that commit. The step is asked for by the stored request: `continueFrom` and
//! `removeEarlierRecords: true`, which the service stores for the start of a follow-up of a run that reported done and for nothing else.
//! A request without the member (a resume, a follow-up of a run that did not report done, a request an older service stored), a request
//! that says `false`, a file a person committed, a records folder the locator names and every checkout in which the step cannot be made
//! safely keep the tree as it was.
//!
//! The runs here are fakes: the whole agent runs in process against the fake AWS and the fake ACP engine. The seeds are built in the
//! test: records committed with the worker's engine identity, other files with a person's. A hook looks at the work tree when the
//! first prompt is written, and may write a file there as an engine would.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::continue_from::ContinueFromHook;
use agentd::direct::hooks::{DirectHook, Event, Hooks, StartupCtx};
use agentd::direct::records;
use agentd::direct::report::plan::{Plan, plan};
use common::git;
use direct_support::{BUCKET, Direct, Opts, PROMPT, RUN, checkout_of_copy, needs_the_base, sha};
use harness_protocol::direct::{
    DirectOutcome, DirectSlice, DirectSummary, TestCommandSource, s3 as keys,
};
use harness_protocol::runtime::{InvocationResponse, RepoSource, TurnOutcome};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SOURCE_RUN: &str = "01M4XSOURCERUN000000000001";

/// The subject of the harness commit; `b866_step_the_subject_is_the_one_the_runs_see` (tests/b866_earlier_records_step.rs) holds it equal
/// to `earlier_records::SUBJECT`.
const SUBJECT: &str = "harness: earlier request's records removed from the work tree";

/// The identity the worker gives the repository, so every commit the engine makes carries it (`workspace::AGENT_AUTHOR`).
const ENGINE: (&str, &str) = ("ham agent", "agent@harness.invalid");
/// The identity of the harness's own commits (`workspace::HARNESS_AUTHOR`).
const HARNESS: (&str, &str) = ("ham harness", "harness@harness.invalid");
/// A person who commits to the repository.
const PERSON: (&str, &str) = ("Pat Person", "pat@example.com");
const HARNESS_LINE: &str =
    "ham harness|harness@harness.invalid|ham harness|harness@harness.invalid";

const CARD: &str = "# Goal\n\nSPECIFIC: split_words returns the words in lower case.\n\n1. `python3 -m pytest -q` -> exit 0\n";
const LOG: &str = "RUN | python3 -m pytest -q | exit 0 | pass 1 fail 0 | 1 passed\n";
const RED: &str = "E   assert ['Hello'] == ['hello']\n";
const NOTE: &str = "# Split and lower\n\nDecision: lower case.\n";
const INDEX: &str = "# Context index\n";
const TEST_COMMAND: &str = "python3 -m pytest -q\n";
/// The card this request writes, when a test has its engine write one.
const OWN_CARD: &str = "# Goal\n\nSPECIFIC: split_words returns the words in UPPER case.\n";

/// The records a run of the product leaves under `base`: the card, the log, a red record and a research note.
fn records_under(base: &str) -> Vec<(String, &'static str)> {
    vec![
        (format!("{base}/lifecycle/GOAL.md"), CARD),
        (format!("{base}/lifecycle/LOG.md"), LOG),
        (format!("{base}/lifecycle/red/01-split-words.txt"), RED),
        (format!("{base}/research/01-split-and-lower.md"), NOTE),
    ]
}

/// What stays in every case: the context index and the project's own command file.
fn kept_under(base: &str) -> Vec<(String, &'static str)> {
    vec![
        (format!("{base}/context/INDEX.md"), INDEX),
        (format!("{base}/test-command"), TEST_COMMAND),
    ]
}

// ---- the seed -----------------------------------------------------------------------------------------------------------------

fn git_as(dir: &Path, author: (&str, &str), committer: (&str, &str), args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", committer.0)
        .env("GIT_COMMITTER_EMAIL", committer.1)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_files<S: AsRef<str>>(repo: &Path, files: &[(S, &str)]) {
    for (rel, text) in files {
        let path = repo.join(rel.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

/// Writes `files` into `repo` and commits everything, with `author` as the author and `committer` as the committer.
fn commit<S: AsRef<str>>(
    repo: &Path,
    files: &[(S, &str)],
    author: (&str, &str),
    committer: (&str, &str),
    message: &str,
) {
    write_files(repo, files);
    git_as(repo, author, committer, &["add", "-A"]);
    git_as(repo, author, committer, &["commit", "-q", "-m", message]);
}

/// The repository the test builds its seed in (the fixture bundled it once already, without records).
fn source_repo(d: &Direct) -> PathBuf {
    d.dir.join("python-calc")
}

fn with_members(d: &mut Direct, members: &Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

/// The member of a follow-up of a run that reported done, as the service stores it: the one request that asks for the step.
fn done() -> Value {
    json!({ "removeEarlierRecords": true })
}

/// The stored request of a follow-up of `seed`, with `extra` members beside `continueFrom`.
fn follow_up_of(d: &mut Direct, seed: &str, extra: &Value) {
    let mut members = json!({ "continueFrom": { "runId": SOURCE_RUN, "headSha": seed } });
    for (k, v) in extra.as_object().unwrap() {
        members[k] = v.clone();
    }
    with_members(d, &members);
}

/// Stages the source repository as the run's `input/repo.bundle`, as the repo-gateway stages the delivered bundle of the source run, and
/// stores the request of a follow-up of its head. Returns the seed and the bundle's bytes.
fn stage(d: &mut Direct, extra: &Value) -> (String, Vec<u8>) {
    let repo = source_repo(d);
    let file = d.dir.join("seed.bundle");
    git(
        &repo,
        &["bundle", "create", "-q", file.to_str().unwrap(), "--all"],
    );
    let bytes = std::fs::read(&file).unwrap();
    d.aws.put(&keys::repo_bundle_key(RUN), bytes.clone());
    let seed = git(&repo, &["rev-parse", "HEAD"]);
    follow_up_of(d, &seed, extra);
    (seed, bytes)
}

/// The options of every run here: the clean-checkout test report is off (the runs here look at the start of a session, not at its end).
fn opts(scenario: &'static str) -> Opts {
    Opts {
        scenario,
        report_tests: Some(false),
        ..Opts::default()
    }
}

/// A follow-up whose seed holds, under `.ham`, the records an engine wrote and the files that stay. Returns the run, the seed and the
/// seed's bundle.
async fn follow_up_with_records(
    scenario: &'static str,
    extra: &Value,
) -> (Direct, String, Vec<u8>) {
    let mut d = Direct::start(opts(scenario)).await;
    let repo = source_repo(&d);
    let mut files = records_under(".ham");
    files.extend(kept_under(".ham"));
    commit(
        &repo,
        &files,
        ENGINE,
        ENGINE,
        "Add split_words (lower case)",
    );
    let (seed, bundle) = stage(&mut d, extra);
    (d, seed, bundle)
}

// ---- what the engine finds at its first prompt ------------------------------------------------------------------------------------

/// The work tree when the first `session/prompt` of the session was written.
#[derive(Clone, Debug, Default)]
struct Seen {
    head: String,
    /// The subject of `HEAD` and its parents.
    subject: String,
    parents: String,
    /// `git status --porcelain` below the folders a records folder may be.
    status: String,
    /// Every path that is no folder below `.ham`, `.cah` and `docs` of the work tree, links included.
    files: Vec<String>,
    /// The folders below them.
    folders: Vec<String>,
    /// Every path of the tree of `HEAD`.
    tracked: Vec<String>,
    calc: Option<String>,
}

const TOPS: [&str; 3] = [".ham", ".cah", "docs"];

fn walk(repo: &Path, rel: &str, files: &mut Vec<String>, folders: &mut Vec<String>) {
    let Ok(meta) = std::fs::symlink_metadata(repo.join(rel)) else {
        return;
    };
    if !meta.file_type().is_dir() {
        files.push(rel.to_string());
        return;
    }
    folders.push(rel.to_string());
    let mut names: Vec<String> = std::fs::read_dir(repo.join(rel))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    for name in names {
        walk(repo, &format!("{rel}/{name}"), files, folders);
    }
}

fn look(repo: &Path) -> Seen {
    let (mut files, mut folders) = (Vec::new(), Vec::new());
    for top in TOPS {
        walk(repo, top, &mut files, &mut folders);
    }
    Seen {
        head: git(repo, &["rev-parse", "HEAD"]),
        subject: git(repo, &["log", "-1", "--format=%s"]),
        parents: git(repo, &["log", "-1", "--format=%P"]),
        status: git(
            repo,
            &["status", "--porcelain", "--", ".ham", ".cah", "docs"],
        ),
        files,
        folders,
        tracked: git(repo, &["ls-tree", "-r", "--name-only", "HEAD"])
            .lines()
            .map(String::from)
            .collect(),
        calc: std::fs::read_to_string(repo.join("src/calc.py")).ok(),
    }
}

#[derive(Default)]
struct Shared {
    repo: Option<PathBuf>,
    seen: Option<Seen>,
    /// Files the hook writes into the work tree at the first prompt, as an engine would in its first turn.
    write: Vec<(String, String)>,
}

struct Probe(Arc<Mutex<Shared>>);

impl DirectHook for Probe {
    fn name(&self) -> &'static str {
        "b866-probe"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().repo = Some(ctx.repo.to_path_buf());
            Ok(())
        })
    }

    fn on_event(&self, event: &Event<'_>) {
        let Event::PromptSent(p) = event else { return };
        let mut s = self.0.lock().unwrap();
        if p.no != 1 || s.seen.is_some() {
            return;
        }
        let repo = s.repo.clone().unwrap();
        s.seen = Some(look(&repo));
        write_files(
            &repo,
            &s.write
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_str()))
                .collect::<Vec<_>>(),
        );
    }
}

/// Runs the invocation with the follow-up module and the probe; `write` is what the probe writes into the work tree at the first prompt.
async fn run_writing(d: &Direct, write: &[(&str, &str)]) -> (InvocationResponse, Option<Seen>) {
    let shared = Arc::new(Mutex::new(Shared {
        write: write
            .iter()
            .map(|(p, t)| ((*p).to_string(), (*t).to_string()))
            .collect(),
        ..Shared::default()
    }));
    let modules: Vec<Arc<dyn DirectHook>> = vec![
        Arc::new(ContinueFromHook::new()),
        Arc::new(Probe(shared.clone())),
    ];
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let seen = shared.lock().unwrap().seen.take();
    (resp, seen)
}

async fn run(d: &Direct) -> (InvocationResponse, Option<Seen>) {
    run_writing(d, &[]).await
}

/// A run that reached its first prompt: the view of the work tree then.
async fn started(d: &Direct) -> Seen {
    let (resp, seen) = run(d).await;
    seen.unwrap_or_else(|| panic!("the engine got no first prompt: {resp:?}"))
}

fn subjects(wt: &Path) -> Vec<String> {
    git(wt, &["log", "--format=%s"])
        .lines()
        .map(String::from)
        .collect()
}

/// How many commits of the history of `HEAD` carry the subject of the harness commit.
fn harness_commits(wt: &Path) -> usize {
    subjects(wt)
        .iter()
        .filter(|s| s.as_str() == SUBJECT)
        .count()
}

fn changed(wt: &Path, from: &str, to: &str) -> Vec<String> {
    let mut v: Vec<String> = git(wt, &["diff", "--name-only", from, to])
        .lines()
        .map(String::from)
        .collect();
    v.sort();
    v
}

/// The lines agentd logged about the step.
fn step_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn earlier records:"))
        .collect()
}

fn paths(files: &[(String, &str)]) -> Vec<String> {
    files.iter().map(|(p, _)| p.clone()).collect()
}

/// Nothing was made: the engine started on the seed, with every file of `files` in the work tree and in the head.
fn assert_untouched(seen: &Seen, seed: &str, files: &[String]) {
    assert_eq!(seen.head, seed, "the engine starts on the seed");
    assert_ne!(seen.subject, SUBJECT);
    for path in files {
        assert!(
            seen.files.contains(path),
            "{path} left the work tree: {:?}",
            seen.files
        );
        assert!(seen.tracked.contains(path), "{path} left the head");
    }
    assert_eq!(seen.status, "", "the tree, the index and the head agree");
}

// ---- the step ------------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_follow_up_starts_with_the_earlier_records_out_of_the_work_tree() {
    let (d, seed, _) = follow_up_with_records("writes_files", &done()).await;
    let repo = source_repo(&d);
    // TRIGGER PRESENT: the seed holds the earlier request's card at the path the goal text names, committed by the engine identity only.
    assert_eq!(
        git(&repo, &["show", &format!("{seed}:.ham/lifecycle/GOAL.md")]),
        CARD.trim()
    );
    assert_eq!(
        git(
            &repo,
            &[
                "log",
                "--format=%ae %ce",
                &seed,
                "--",
                ".ham/lifecycle/GOAL.md"
            ]
        ),
        format!("{0} {0}", ENGINE.1)
    );
    let (resp, seen) = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let seen = seen.expect("the engine got a first prompt");

    // At the first prompt: one commit of the harness on the seed, and none of the records in the work tree or in its tree.
    assert_eq!(seen.parents, seed, "the seed is the only parent");
    assert_eq!(seen.subject, SUBJECT);
    for path in paths(&records_under(".ham")) {
        assert!(
            !seen.files.contains(&path),
            "{path} is in the work tree: {:?}",
            seen.files
        );
        assert!(!seen.tracked.contains(&path), "{path} is in the head");
    }
    assert_eq!(
        seen.files,
        paths(&kept_under(".ham")),
        "the context index and the command file stay, and nothing else is below .ham"
    );
    assert_eq!(
        seen.folders,
        [".ham", ".ham/context"],
        "the folders that became empty are gone"
    );
    for path in paths(&kept_under(".ham")) {
        assert!(seen.tracked.contains(&path), "{path}");
    }
    assert_eq!(
        seen.status, "",
        "the tree, the index and the head agree below the records"
    );

    let wt = d.worktree();
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%an|%ae|%cn|%ce", &seen.head]),
        HARNESS_LINE
    );
    // Nothing is lost: every record is in the parent of the commit, byte for byte.
    for path in paths(&records_under(".ham")) {
        assert_eq!(
            git(&wt, &["rev-parse", &format!("{}^:{path}", seen.head)]),
            git(&repo, &["rev-parse", &format!("{seed}:{path}")]),
            "{path}"
        );
    }
    assert_eq!(
        changed(&wt, &seed, &seen.head),
        paths(&records_under(".ham")),
        "the commit removes the records and touches nothing else"
    );
    // The engine receives the user's text as typed: nothing about the earlier run is added to it.
    assert_eq!(d.prompts_received(), [PROMPT]);
    assert_eq!(git(&wt, &["status", "--porcelain"]), "", "after the run");
    assert_eq!(harness_commits(&wt), 1);
    println!(
        "B866 TRIGGER PRESENT seed={} earlier-card=in-the-seed first-prompt-on={} records=out-of-the-work-tree",
        &seed[..12],
        &seen.head[..12]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_commit_is_the_base_of_the_runs_work() {
    let (with, seed, bundle) = follow_up_with_records("writes_files", &done()).await;
    let (resp, _) = run(&with).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = with.summary();
    let wt = with.worktree();
    let base = s.git.base_sha.clone().expect("the run has a base");
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%s|%P", &base]),
        format!("{SUBJECT}|{seed}"),
        "the base is the harness commit on the seed"
    );
    let head = s.git.head_sha.clone().expect("the run delivered a head");
    assert!(
        s.git.files_changed >= 1,
        "TRIGGER: the engine changed files"
    );
    let files = changed(&wt, &base, &head);
    assert_eq!(files.len() as u32, s.git.files_changed, "{files:?}");
    assert!(
        files.iter().all(|f| !f.starts_with(".ham/")),
        "no record is counted as the run's change: {files:?}"
    );
    // What the review round and the summary list (`workspace::changed_between` from the run's base) names no record either.
    let tree = agentd::workspace::Worktree {
        root: wt.parent().unwrap().to_path_buf(),
        repo: wt.clone(),
        branch: format!("harness/{RUN}/direct"),
    };
    let listed = agentd::workspace::changed_between(&tree, &base)
        .await
        .unwrap();
    assert!(listed.iter().all(|f| !f.starts_with(".ham/")), "{listed:?}");

    // The same follow-up from the same seed with the records kept changes the same number of files.
    let mut kept = Direct::start(opts("writes_files")).await;
    kept.aws.put(&keys::repo_bundle_key(RUN), bundle);
    follow_up_of(&mut kept, &seed, &json!({ "removeEarlierRecords": false }));
    let (resp, _) = run(&kept).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let k = kept.summary();
    assert_eq!(k.git.base_sha.as_deref(), Some(seed.as_str()));
    assert_eq!(s.git.files_changed, k.git.files_changed);
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_same_seed_gives_the_same_commit() {
    let (one, seed, bundle) = follow_up_with_records("end_turn_done", &done()).await;
    let first = started(&one).await;
    // A second run in a new machine, from the same bundle.
    let mut two = Direct::start(opts("end_turn_done")).await;
    two.aws.put(&keys::repo_bundle_key(RUN), bundle);
    follow_up_of(&mut two, &seed, &done());
    let second = started(&two).await;
    assert_eq!(first.subject, SUBJECT, "TRIGGER: the commit was made");
    assert_eq!(
        first.head, second.head,
        "the same seed gives the same commit"
    );
    // The harness is the author and the committer, and both dates are the commit time of the seed.
    let wt = one.worktree();
    let when = git(&wt, &["log", "-1", "--format=%ct", &seed]);
    assert_eq!(
        git(
            &wt,
            &["log", "-1", "--format=%an|%ae|%cn|%ce|%at|%ct", &first.head]
        ),
        format!("{HARNESS_LINE}|{when}|{when}")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_request_that_says_false_keeps_the_records() {
    let (d, seed, _) =
        follow_up_with_records("writes_files", &json!({ "removeEarlierRecords": false })).await;
    let (resp, seen) = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let seen = seen.expect("the engine got a first prompt");
    let mut all = paths(&records_under(".ham"));
    all.extend(paths(&kept_under(".ham")));
    assert_untouched(&seen, &seed, &all);
    assert_eq!(
        std::fs::read_to_string(d.worktree().join(".ham/lifecycle/GOAL.md")).unwrap(),
        CARD,
        "the card is the open task's own and stays"
    );
    assert_eq!(harness_commits(&d.worktree()), 0);
    assert_eq!(d.summary().git.base_sha.as_deref(), Some(seed.as_str()));
    println!(
        "B866 NEGATIVE PRESENT removeEarlierRecords=false first-prompt-on=seed({}) records=kept",
        &seed[..12]
    );
}

/// The lead's ruling 2: an absent member keeps the records. It is the shape a service of release 5.24 stores for a follow-up, and the
/// shape this release stores for every resume, for a follow-up of a run that did not report done and for evidence the store refuses.
#[tokio::test(flavor = "multi_thread")]
async fn b866_r2_a_follow_up_without_the_member_keeps_every_record_and_gets_no_commit() {
    let (d, seed, bundle) = follow_up_with_records("writes_files", &json!({})).await;
    let stored: Value = serde_json::from_slice(&d.request_json).unwrap();
    assert!(
        stored.get("continueFrom").is_some() && stored.get("removeEarlierRecords").is_none(),
        "TRIGGER: the stored request is a follow-up without the member: {stored}"
    );
    let (resp, seen) = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let seen = seen.expect("the engine got a first prompt");
    let mut all = paths(&records_under(".ham"));
    all.extend(paths(&kept_under(".ham")));
    assert_untouched(&seen, &seed, &all);
    let wt = d.worktree();
    for (path, text) in records_under(".ham") {
        assert_eq!(
            std::fs::read_to_string(wt.join(&path)).unwrap(),
            text,
            "{path} is as the earlier run delivered it"
        );
    }
    assert_eq!(harness_commits(&wt), 0, "no commit of the harness");
    assert_eq!(d.summary().git.base_sha.as_deref(), Some(seed.as_str()));
    let lines = step_lines(&d);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("not removed:") && l.contains("removeEarlierRecords is not true")),
        "the log says that the request does not ask for the step: {lines:?}"
    );
    // TRIGGER: the same seed with the service's `true` does get the commit.
    let mut on = Direct::start(opts("writes_files")).await;
    on.aws.put(&keys::repo_bundle_key(RUN), bundle);
    follow_up_of(&mut on, &seed, &done());
    assert_eq!(started(&on).await.subject, SUBJECT);
    println!(
        "B866 NEGATIVE PRESENT removeEarlierRecords=absent first-prompt-on=seed({}) records=kept",
        &seed[..12]
    );
}

/// The member means nothing without `continueFrom`: a first request whose stored request says `true` starts as any first request, on a
/// tree that holds records. The service never stores that request; the worker does nothing with it.
#[tokio::test(flavor = "multi_thread")]
async fn b866_r2_true_without_continue_from_takes_nothing_out() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    let mut files = records_under(".ham");
    files.extend(kept_under(".ham"));
    commit(
        &repo,
        &files,
        ENGINE,
        ENGINE,
        "records in the repository a first request starts on",
    );
    let file = d.dir.join("seed.bundle");
    git(
        &repo,
        &["bundle", "create", "-q", file.to_str().unwrap(), "--all"],
    );
    d.aws
        .put(&keys::repo_bundle_key(RUN), std::fs::read(&file).unwrap());
    let head = git(&repo, &["rev-parse", "HEAD"]);
    with_members(&mut d, &json!({ "removeEarlierRecords": true }));
    let stored: Value = serde_json::from_slice(&d.request_json).unwrap();
    assert!(
        stored.get("continueFrom").is_none() && stored["removeEarlierRecords"] == json!(true),
        "TRIGGER: a first request that says true: {stored}"
    );
    let seen = started(&d).await;
    for path in paths(&files) {
        assert!(
            seen.files.contains(&path),
            "{path} left the work tree: {:?}",
            seen.files
        );
        assert!(seen.tracked.contains(&path), "{path} left the head");
    }
    let wt = d.worktree();
    assert_eq!(harness_commits(&wt), 0);
    assert!(
        git(&wt, &["merge-base", "--is-ancestor", &head, "HEAD"]).is_empty(),
        "the run works on the repository it was given"
    );
    assert!(
        step_lines(&d).is_empty(),
        "the step is not asked for a first request: {:?}",
        step_lines(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_seed_without_records_gets_no_commit() {
    // no records folder at all; only the files that stay; records switched off in the locator, with files in the folder
    type Case = (&'static str, Vec<(String, &'static str)>);
    let mut off = records_under(".ham");
    off.push((".ham/records".to_string(), "off\n"));
    let cases: Vec<Case> = vec![
        ("no .ham", Vec::new()),
        ("only what stays", kept_under(".ham")),
        ("off", off),
    ];
    for (label, files) in cases {
        let mut d = Direct::start(opts("end_turn_done")).await;
        if !files.is_empty() {
            commit(&source_repo(&d), &files, ENGINE, ENGINE, "the earlier run");
        }
        let (seed, _) = stage(&mut d, &done());
        let seen = started(&d).await;
        assert_untouched(&seen, &seed, &paths(&files));
        assert_eq!(harness_commits(&d.worktree()), 0, "{label}");
        assert_eq!(
            d.summary().git.base_sha.as_deref(),
            Some(seed.as_str()),
            "{label}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_folder_the_locator_names_is_left_alone() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let mut files = records_under("docs");
    files.push((".ham/records".to_string(), "docs\n"));
    commit(&source_repo(&d), &files, ENGINE, ENGINE, "the earlier run");
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_untouched(&seen, &seed, &paths(&files));
    assert_eq!(harness_commits(&d.worktree()), 0);
    let lines = step_lines(&d);
    assert!(
        lines.iter().any(|l| l.contains("not removed:")
            && l.contains("the layout is not handled")
            && l.contains("docs")),
        "the log says the layout is not handled: {lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_an_invalid_locator_leaves_the_tree_alone() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let mut files = records_under(".ham");
    files.push((".ham/records".to_string(), "../x\n"));
    commit(&source_repo(&d), &files, ENGINE, ENGINE, "the earlier run");
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_untouched(&seen, &seed, &paths(&files));
    assert_eq!(harness_commits(&d.worktree()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_earlier_folder_is_handled_in_its_own_place() {
    let mut d = Direct::start(opts("writes_files")).await;
    let mut files = records_under(".cah");
    files.push((".cah/context/INDEX.md".to_string(), INDEX));
    commit(&source_repo(&d), &files, ENGINE, ENGINE, "the earlier run");
    let (seed, _) = stage(&mut d, &done());
    let (resp, seen) = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let seen = seen.expect("the engine got a first prompt");
    assert_eq!(seen.subject, SUBJECT, "the records left the earlier folder");
    assert_eq!(seen.parents, seed);
    assert_eq!(
        seen.files,
        [".cah/context/INDEX.md"],
        "the records are gone, the index stays and nothing was made under .ham"
    );
    assert_eq!(seen.status, "");
    // The run still works in the earlier folder: the first line of the first prompt names it, and the folder is chosen again alike.
    let prompts = d.prompts_received();
    assert!(
        prompts[0].starts_with("Records folder of this run: .cah"),
        "{}",
        prompts[0]
    );
    assert_eq!(records::choose(&d.worktree()), Ok(".cah"));
    assert!(!d.worktree().join(".ham").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_last_state_of_the_earlier_folder_is_never_taken_out() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let files = records_under(".cah");
    commit(&source_repo(&d), &files, ENGINE, ENGINE, "the earlier run");
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_untouched(&seen, &seed, &paths(&files));
    assert_eq!(harness_commits(&d.worktree()), 0);
    let lines = step_lines(&d);
    assert!(
        lines.iter().any(|l| l.contains("not removed:")
            && l.contains("the last state")
            && l.contains(".cah")),
        "the log names the reason: {lines:?}"
    );
    assert_eq!(records::choose(&d.worktree()), Ok(".cah"));
}

// ---- a later session and a second invocation ---------------------------------------------------------------------------------------------

/// What session 1 of a follow-up left: its bundle, the head it delivered and its base.
struct Session1 {
    seed: String,
    seed_bundle: Vec<u8>,
    bundle: Vec<u8>,
    head: String,
    base: String,
}

/// Session 1 of a follow-up with records, to its end; `write` is what its engine writes at the first prompt beside the scenario's own files.
async fn first_session(scenario: &'static str, write: &[(&str, &str)]) -> Session1 {
    let (d, seed, seed_bundle) = follow_up_with_records(scenario, &done()).await;
    let (resp, seen) = run_writing(&d, write).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        seen.expect("a first prompt").subject,
        SUBJECT,
        "TRIGGER: session 1 made the harness commit"
    );
    let s = d.summary();
    Session1 {
        seed,
        seed_bundle,
        bundle: d
            .aws
            .get(&keys::bundle_key(RUN))
            .expect("session 1 bundled"),
        head: s.git.head_sha.clone().unwrap(),
        base: s.git.base_sha.clone().unwrap(),
    }
}

/// Session 2 in a new machine: the invocation names `restore` as the bundle and `restore_head` as the commit the repository must be at, and
/// the base the service knows, which for a follow-up is the seed.
async fn second_session(
    scenario: &'static str,
    one: &Session1,
    restore: Vec<u8>,
    restore_head: &str,
) -> Direct {
    let mut d2 = Direct::start(opts(scenario)).await;
    follow_up_of(&mut d2, &one.seed, &done());
    d2.aws.put(&keys::bundle_key_t(RUN, 1), restore);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(one.seed.clone()),
        restore_head: Some(restore_head.to_string()),
        sessions_before: Some(1),
        brief: None,
    });
    d2
}

fn summary_of_session(d: &Direct, turn: u32) -> DirectSummary {
    serde_json::from_slice(&d.aws.get(&keys::summary_key_t(RUN, turn)).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_later_session_finds_the_commit_and_makes_none() {
    // Session 1 wrote this request's own card.
    let one = first_session("writes_files", &[(".ham/lifecycle/GOAL.md", OWN_CARD)]).await;
    assert_ne!(
        one.head, one.base,
        "TRIGGER: session 1 left work on the harness commit"
    );
    let d2 = second_session("writes_files", &one, one.bundle.clone(), &one.head).await;
    let seen = started(&d2).await;
    assert_eq!(
        seen.head, one.head,
        "session 2 starts on the work it restored"
    );
    assert_eq!(
        harness_commits(&d2.worktree()),
        1,
        "session 2 made no commit"
    );
    assert!(
        seen.files.contains(&".ham/lifecycle/GOAL.md".to_string()),
        "the card this request wrote is still there: {:?}",
        seen.files
    );
    assert_eq!(
        git(
            &d2.worktree(),
            &["show", &format!("{}:.ham/lifecycle/GOAL.md", one.head)]
        ),
        OWN_CARD.trim()
    );
    let s = summary_of_session(&d2, 2);
    assert_eq!(
        s.git.base_sha.as_deref(),
        Some(one.base.as_str()),
        "the work of the run is measured from the harness commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_later_session_on_the_original_bundle_makes_the_commit_again() {
    let one = first_session("end_turn_done", &[]).await;
    // Session 1 left nothing the service could use: session 2 starts again from the original bundle, at the seed.
    let d2 = second_session("end_turn_done", &one, one.seed_bundle.clone(), &one.seed).await;
    let seen = started(&d2).await;
    assert_eq!(seen.subject, SUBJECT);
    assert_eq!(seen.parents, one.seed);
    assert_eq!(seen.head, one.base, "the same seed gives the same commit");
    assert_eq!(seen.files, paths(&kept_under(".ham")));
    assert_eq!(harness_commits(&d2.worktree()), 1);
    assert_eq!(
        summary_of_session(&d2, 2).git.base_sha.as_deref(),
        Some(one.base.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_step_reads_the_seed_and_not_the_work_tree() {
    // The engine of session 1 pointed the locator at another folder: the work session 2 restores says `docs`.
    let one = first_session("writes_files", &[(".ham/records", "docs\n")]).await;
    let d2 = second_session("writes_files", &one, one.bundle.clone(), &one.head).await;
    let seen = started(&d2).await;
    assert!(
        seen.files.contains(&".ham/records".to_string()),
        "TRIGGER: the restored work holds the changed locator: {:?}",
        seen.files
    );
    assert_eq!(seen.head, one.head);
    assert_eq!(harness_commits(&d2.worktree()), 1);
    assert_eq!(
        summary_of_session(&d2, 2).git.base_sha.as_deref(),
        Some(one.base.as_str()),
        "the step still computes the commit of the seed, and it is the base"
    );
}

/// The file the tripwire leaves in the configuration directory after its first passage.
fn marker(d: &Direct) -> PathBuf {
    d.agent_data().join("config/agentd-continue-from.json")
}

fn remove_marker(d: &Direct) {
    let file = marker(d);
    assert!(
        file.is_file(),
        "TRIGGER: the first invocation wrote the marker"
    );
    // The configuration directory may be closed to writes; the test opens it for the removal and closes it again.
    let dir = file.parent().unwrap();
    let before = std::fs::metadata(dir).unwrap().permissions();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::set_permissions(dir, before).unwrap();
    assert!(!file.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_second_invocation_after_the_commit_and_before_the_marker_starts() {
    let (d, seed, _) = follow_up_with_records("end_turn_done", &done()).await;
    let first = started(&d).await;
    let wt = d.worktree();
    assert_eq!(first.subject, SUBJECT);
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        first.head,
        "TRIGGER: the checkout is at the harness commit (the engine changed nothing)"
    );
    // As if the first invocation had ended after the commit and before the tripwire wrote its file.
    remove_marker(&d);
    let (second, seen) = run(&d).await;
    // The second invocation started and ran the engine: a refusal by the tripwire ends the run before any prompt, with no stop reason.
    assert_eq!(
        second.stop_reason.as_deref(),
        Some("end_turn"),
        "{second:?}"
    );
    let seen = seen.expect("the engine got a first prompt again");
    assert_eq!(seen.head, first.head, "no second commit");
    assert_eq!(harness_commits(&wt), 1);
    assert_eq!(git(&wt, &["log", "-1", "--format=%P", &seen.head]), seed);
    assert!(
        marker(&d).is_file(),
        "the tripwire wrote its file this time"
    );
    assert_eq!(
        d.summary().git.base_sha.as_deref(),
        Some(first.head.as_str()),
        "the base is the harness commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_reused_checkout_ahead_of_the_harness_commit_is_still_refused() {
    let (d, _, _) = follow_up_with_records("end_turn_done", &done()).await;
    let first = started(&d).await;
    assert_eq!(
        first.subject, SUBJECT,
        "TRIGGER: the harness commit was made"
    );
    let wt = d.worktree();
    // One more commit on top of the harness commit, and no marker: nothing says this checkout passed the check once.
    commit(&wt, &[("NOTES.md", "more\n")], ENGINE, ENGINE, "one more");
    let now = git(&wt, &["rev-parse", "HEAD"]);
    assert_ne!(now, first.head);
    remove_marker(&d);
    let prompts_before = d.prompts_received().len();
    let (second, seen) = run(&d).await;
    assert_ne!(second.outcome, TurnOutcome::Completed, "{second:?}");
    assert_eq!(second.stop_reason, None, "no engine turn: {second:?}");
    assert!(seen.is_none(), "the engine got no prompt");
    assert_eq!(d.prompts_received().len(), prompts_before);
    assert!(!marker(&d).exists());
    // The engine never started in the second invocation: the tripwire ended it before the engine's process existed. (The end of a second
    // invocation is the end sequence's conflict over the first one's objects, which takes the place of the tripwire's words in the outcome.)
    let log = d.log.lines();
    let starts = log
        .iter()
        .filter(|l| l.contains("direct turn engine starting"))
        .count();
    assert_eq!(
        starts, 1,
        "the engine started in the first invocation only: {log:#?}"
    );
}

// ---- what the run measures and what stays ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_follow_up_in_which_the_engine_changes_nothing() {
    let (d, seed, _) = follow_up_with_records("end_turn_done", &done()).await;
    let (_, seen) = run(&d).await;
    let seen = seen.expect("a first prompt");
    assert_eq!(
        seen.subject, SUBJECT,
        "TRIGGER: the harness commit was made"
    );
    let s = d.summary();
    assert_eq!(s.git.files_changed, 0);
    assert_eq!(s.outcome, DirectOutcome::StoppedNoChanges, "{s:?}");
    assert_eq!(s.git.base_sha.as_deref(), Some(seen.head.as_str()));
    assert_eq!(
        s.git.base_sha, s.git.head_sha,
        "nothing but the harness commit is on the seed"
    );
    assert_eq!(
        git(&d.worktree(), &["log", "-1", "--format=%P", &seen.head]),
        seed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_saved_copy_of_a_follow_up_holds_the_commit_and_no_earlier_record() {
    // The copy of the work that the worker saves (the one `ham pull <runId> --snapshot` downloads) is made on top of the harness commit.
    let (d, seed, _) = follow_up_with_records("writes_files", &done()).await;
    let (resp, seen) = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let commit = seen.expect("a first prompt").head;
    // A copy holds what the run changed since it started, and a follow-up starts at the head it continues from (the seed, the
    // one head of the bundle the run started from), not at the harness commit. So the copy leaves the seed out and holds the harness
    // commit itself: the commit that takes the earlier records out is part of what this run changed.
    let stored = d
        .aws
        .get(&keys::snapshot_bundle_key(RUN))
        .expect("TRIGGER: the run saved a copy of its work");
    assert!(
        needs_the_base(&stored),
        "TRIGGER: the saved copy of a follow-up leaves out what the run it continues delivered"
    );
    let needed = harness_protocol::bundle_header::needs(&stored[..stored.len().min(65_536)])
        .expect("a bundle header");
    assert_eq!(
        needed,
        vec![seed.clone()],
        "the copy needs the seed and nothing else: the harness commit is in the copy"
    );
    // A reader takes the bundle the run started from first and fetches the copy into it, as `ham pull <runId> --snapshot` says.
    let copy = checkout_of_copy(&d.aws, &d.dir, &keys::snapshot_bundle_key(RUN), "copy");
    // The harness commit and the seed are in the copy's history (the helper fails when git says they are not).
    git(&copy, &["merge-base", "--is-ancestor", &commit, "HEAD"]);
    git(&copy, &["merge-base", "--is-ancestor", &seed, "HEAD"]);
    for path in paths(&records_under(".ham")) {
        assert!(!copy.join(&path).exists(), "{path} is in the saved copy");
        // Nothing is lost: the record is in the copy's history, at the seed.
        git(&copy, &["cat-file", "-e", &format!("{seed}:{path}")]);
    }
    for path in paths(&kept_under(".ham")) {
        assert!(
            copy.join(&path).is_file(),
            "{path} is not in the saved copy"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_link_below_the_records_is_removed_and_never_followed() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    write_files(&repo, &records_under(".ham"));
    std::os::unix::fs::symlink("../../../src/calc.py", repo.join(".ham/lifecycle/red/link"))
        .unwrap();
    commit(
        &repo,
        &[] as &[(&str, &str)],
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    assert_eq!(
        git(&repo, &["ls-files", "-s", ".ham/lifecycle/red/link"])
            .split(' ')
            .next(),
        Some("120000"),
        "TRIGGER: the link is tracked as a link"
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(seen.subject, SUBJECT);
    assert_eq!(seen.parents, seed);
    assert!(
        seen.files.is_empty(),
        "the link left the tree with the records: {:?}",
        seen.files
    );
    assert!(std::fs::symlink_metadata(d.worktree().join(".ham/lifecycle/red/link")).is_err());
    assert_eq!(
        seen.calc.as_deref(),
        Some(common::BASE_CALC),
        "the link's target is not touched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_file_a_person_committed_stays() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    // a note a person wrote; a person's document the engine later edited; a file whose commit a person made for the engine
    commit(
        &repo,
        &[
            (".ham/research/team.md", "# The team's note\n"),
            (".ham/research/mixed.md", "# A person's document\n"),
        ],
        PERSON,
        PERSON,
        "our notes",
    );
    commit(
        &repo,
        &[
            (".ham/lifecycle/GOAL.md", CARD),
            (
                ".ham/research/mixed.md",
                "# A person's document\n\nAn edit by the engine.\n",
            ),
        ],
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    commit(
        &repo,
        &[(".ham/research/picked.md", "# Picked\n")],
        ENGINE,
        PERSON,
        "picked by a person",
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(seen.subject, SUBJECT);
    assert_eq!(seen.parents, seed);
    let stay = [
        ".ham/research/mixed.md",
        ".ham/research/picked.md",
        ".ham/research/team.md",
    ];
    assert_eq!(
        seen.files, stay,
        "the card is gone and a person's files stay"
    );
    let wt = d.worktree();
    for path in stay {
        assert_eq!(
            git(&wt, &["rev-parse", &format!("{}:{path}", seen.head)]),
            git(&repo, &["rev-parse", &format!("{seed}:{path}")]),
            "{path} is in the head with the seed's content"
        );
    }
    assert_eq!(changed(&wt, &seed, &seen.head), [".ham/lifecycle/GOAL.md"]);
    assert_eq!(seen.status, "");
}

/// `git mv` and a commit of it, as the engine makes them in its shell.
fn engine_moves(repo: &Path, from: &str, to: &str, message: &str) {
    std::fs::create_dir_all(repo.join(to).parent().unwrap()).unwrap();
    git_as(repo, ENGINE, ENGINE, &["mv", from, to]);
    git_as(repo, ENGINE, ENGINE, &["commit", "-q", "-m", message]);
}

/// A person's note of a few lines, so that a rename with an edit is still told as a rename.
const TEAM_NOTE: &str = "# The team's note\n\nWe split on whitespace.\nWe keep the order of the words.\nA blank text gives an empty list.\nNobody changes this without asking Pat.\n";

/// A file whose content a person committed stays when the engine renamed it inside the two folders (to fit the numbering of the notes).
/// The new path has only the engine's commit in its own history, so the history of the path it was renamed from is read too.
#[tokio::test(flavor = "multi_thread")]
async fn b866_a_persons_file_the_engine_renamed_inside_the_folders_stays() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    commit(
        &repo,
        &[(".ham/research/my-notes.md", TEAM_NOTE)],
        PERSON,
        PERSON,
        "our notes",
    );
    let person_blob = git(&repo, &["rev-parse", "HEAD:.ham/research/my-notes.md"]);
    commit(
        &repo,
        &[(".ham/lifecycle/GOAL.md", CARD)],
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    engine_moves(
        &repo,
        ".ham/research/my-notes.md",
        ".ham/research/01-my-notes.md",
        "Number the notes",
    );
    // TRIGGER PRESENT: the path's own history holds the engine's commit only, and its content is the person's.
    assert_eq!(
        git(
            &repo,
            &[
                "log",
                "--format=%ae %ce",
                "--",
                ".ham/research/01-my-notes.md"
            ]
        ),
        format!("{0} {0}", ENGINE.1)
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD:.ham/research/01-my-notes.md"]),
        person_blob
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(
        seen.subject, SUBJECT,
        "TRIGGER PRESENT: the step ran and took the card out"
    );
    assert_eq!(seen.parents, seed);
    assert_eq!(
        seen.files,
        [".ham/research/01-my-notes.md"],
        "the card is gone and the person's note stays where the engine put it"
    );
    let wt = d.worktree();
    assert_eq!(
        git(
            &wt,
            &[
                "rev-parse",
                &format!("{}:.ham/research/01-my-notes.md", seen.head)
            ]
        ),
        person_blob,
        "the note is in the head with the person's content"
    );
    assert_eq!(changed(&wt, &seed, &seen.head), [".ham/lifecycle/GOAL.md"]);
    assert_eq!(seen.status, "");
    println!(
        "B866 TRIGGER PRESENT a person's note the engine renamed inside the folders: kept at {}",
        seen.files[0]
    );
}

/// The same for a person's document that the engine moved into one of the two folders from elsewhere, with an edit of its own.
#[tokio::test(flavor = "multi_thread")]
async fn b866_a_persons_file_the_engine_moved_into_the_folders_stays() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    commit(
        &repo,
        &[
            ("docs/notes.md", TEAM_NOTE),
            (
                "docs/decisions.md",
                "# Decisions\n\nLower case, decided in March.\nPat signs off every change.\nThe list is kept in order.\nOld entries are never removed.\n",
            ),
        ],
        PERSON,
        PERSON,
        "our documents",
    );
    commit(
        &repo,
        &[(".ham/lifecycle/GOAL.md", CARD)],
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    // A move as it is, and a move with an edit in the same commit.
    engine_moves(
        &repo,
        "docs/notes.md",
        ".ham/research/notes.md",
        "File the notes under research",
    );
    std::fs::create_dir_all(repo.join(".ham/research")).unwrap();
    git_as(
        &repo,
        ENGINE,
        ENGINE,
        &["mv", "docs/decisions.md", ".ham/research/02-decisions.md"],
    );
    let edited = format!(
        "{}An entry by the engine.\n",
        std::fs::read_to_string(repo.join(".ham/research/02-decisions.md")).unwrap()
    );
    commit(
        &repo,
        &[(".ham/research/02-decisions.md", edited.as_str())],
        ENGINE,
        ENGINE,
        "File the decisions under research and add one",
    );
    for path in [".ham/research/notes.md", ".ham/research/02-decisions.md"] {
        assert_eq!(
            git(&repo, &["log", "--format=%ae %ce", "--", path]),
            format!("{0} {0}", ENGINE.1),
            "TRIGGER PRESENT: the history of {path} itself holds the engine's commit only"
        );
    }
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(seen.subject, SUBJECT, "TRIGGER PRESENT: the step ran");
    assert_eq!(
        seen.files,
        [".ham/research/02-decisions.md", ".ham/research/notes.md"],
        "the card is gone and both documents of a person stay"
    );
    let wt = d.worktree();
    for path in [".ham/research/notes.md", ".ham/research/02-decisions.md"] {
        assert_eq!(
            git(&wt, &["rev-parse", &format!("{}:{path}", seen.head)]),
            git(&repo, &["rev-parse", &format!("{seed}:{path}")]),
            "{path} is in the head with the seed's content"
        );
    }
    assert_eq!(changed(&wt, &seed, &seen.head), [".ham/lifecycle/GOAL.md"]);
    assert_eq!(seen.status, "");
}

/// A record the engine wrote and later renamed is still the product's: the history of the path it was renamed from holds the engine only.
#[tokio::test(flavor = "multi_thread")]
async fn b866_a_record_of_the_engine_that_the_engine_renamed_still_goes() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    commit(
        &repo,
        &[
            (".ham/lifecycle/GOAL.md", CARD),
            (".ham/research/split.md", NOTE),
        ],
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    engine_moves(
        &repo,
        ".ham/research/split.md",
        ".ham/research/01-split-and-lower.md",
        "Number the note",
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(seen.subject, SUBJECT);
    assert_eq!(seen.parents, seed);
    assert!(
        seen.files.is_empty(),
        "both records leave the tree: {:?}",
        seen.files
    );
    assert_eq!(
        changed(&d.worktree(), &seed, &seen.head),
        [
            ".ham/lifecycle/GOAL.md",
            ".ham/research/01-split-and-lower.md"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_only_a_persons_files_means_no_commit() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let files = records_under(".ham");
    commit(
        &source_repo(&d),
        &files,
        PERSON,
        PERSON,
        "records written by hand",
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_untouched(&seen, &seed, &paths(&files));
    assert_eq!(harness_commits(&d.worktree()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_a_third_request_loses_the_second_requests_card() {
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    // The first request's card, the harness commit of the second request that took it out, and the second request's own card.
    commit(
        &repo,
        &[(".ham/lifecycle/GOAL.md", CARD)],
        ENGINE,
        ENGINE,
        "the first request",
    );
    git_as(
        &repo,
        HARNESS,
        HARNESS,
        &["rm", "-q", "-r", ".ham/lifecycle"],
    );
    git_as(&repo, HARNESS, HARNESS, &["commit", "-q", "-m", SUBJECT]);
    commit(
        &repo,
        &[(".ham/lifecycle/GOAL.md", OWN_CARD)],
        ENGINE,
        ENGINE,
        "the second request",
    );
    assert_eq!(
        git(
            &repo,
            &["log", "--format=%ae", "--", ".ham/lifecycle/GOAL.md"]
        ),
        format!("{0}\n{1}\n{0}", ENGINE.1, HARNESS.1),
        "TRIGGER: the harness touched the path between the two cards"
    );
    let (seed, _) = stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(
        seen.subject, SUBJECT,
        "the harness identity counts as the product's"
    );
    assert_eq!(seen.parents, seed);
    assert!(seen.files.is_empty(), "{:?}", seen.files);
    assert_eq!(
        git(
            &d.worktree(),
            &["show", &format!("{}^:.ham/lifecycle/GOAL.md", seen.head)]
        ),
        OWN_CARD.trim(),
        "the second request's card is one step back in the history"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b866_the_report_plan_of_a_quick_follow_up_reads_no_earlier_log() {
    // The seed's log names a test command; the project has no command file, and its tests sit in a plain `tests/test_*.py` layout.
    let mut d = Direct::start(opts("end_turn_done")).await;
    let repo = source_repo(&d);
    commit(
        &repo,
        &records_under(".ham"),
        ENGINE,
        ENGINE,
        "the earlier run",
    );
    let before = match plan(&repo) {
        Plan::Steps(steps) => steps,
        Plan::NoCommand(why) => panic!("no steps on the seed: {why}"),
    };
    assert!(
        before
            .iter()
            .any(|s| s.source == TestCommandSource::LifecycleLog && s.logged.is_some()),
        "TRIGGER: on the seed's tree the report takes its command from the earlier log: {before:?}"
    );
    stage(&mut d, &done());
    let seen = started(&d).await;
    assert_eq!(seen.subject, SUBJECT);
    let after = match plan(&d.worktree()) {
        Plan::Steps(steps) => steps,
        Plan::NoCommand(why) => panic!("no steps after the step: {why}"),
    };
    assert!(!after.is_empty());
    for step in &after {
        assert_eq!(step.source, TestCommandSource::Detected, "{step:?}");
        assert_eq!(step.logged, None, "{step:?}");
    }
}

// ---- a checkout the step cannot be made in safely, through a whole run -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b866_an_unsafe_checkout_is_left_as_it_was() {
    // A submodule entry below the records: the step does nothing, says so in one line, and the run goes on.
    let mut d = Direct::start(opts("writes_files")).await;
    let repo = source_repo(&d);
    let files = records_under(".ham");
    write_files(&repo, &files);
    git_as(&repo, ENGINE, ENGINE, &["add", "-A"]);
    let some_commit = git(&repo, &["rev-parse", "HEAD"]);
    git_as(
        &repo,
        ENGINE,
        ENGINE,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{some_commit},.ham/lifecycle/sub"),
        ],
    );
    git_as(
        &repo,
        ENGINE,
        ENGINE,
        &["commit", "-q", "-m", "the earlier run"],
    );
    assert!(
        git(
            &repo,
            &["ls-tree", "-r", "HEAD", "--", ".ham/lifecycle/sub"]
        )
        .starts_with("160000 commit"),
        "TRIGGER: a submodule entry is below the records"
    );
    let (seed, _) = stage(&mut d, &done());
    let (resp, seen) = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "the run goes on: {resp:?}"
    );
    let seen = seen.expect("a first prompt");
    assert_untouched(&seen, &seed, &paths(&files));
    assert_eq!(harness_commits(&d.worktree()), 0);
    let lines = step_lines(&d);
    assert_eq!(lines.len(), 1, "one line: {lines:?}");
    assert!(
        lines[0].starts_with("direct turn earlier records: not removed:")
            && lines[0].contains("submodule"),
        "{lines:?}"
    );
}
