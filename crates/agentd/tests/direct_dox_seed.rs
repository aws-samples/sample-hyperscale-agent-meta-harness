//! The DOX starter is written into the work tree and committed before the first prompt of a run (`src/direct/dox.rs`, step 4 of
//! `start_and_prompt`). The engine loads a root `AGENTS.md` only if it exists when its session starts, so a file Cline writes at the end of a run
//! never reaches that run, and a run that ends early leaves no docs. The harness adds no check and no gate: the engine and the model keep the
//! docs. The commit comes before the base is read, so the starter is never the run's own change.
//!
//! The deliveries here are fakes: the whole agent runs in process against the fake AWS and the fake ACP engine, with the real delivery skill of the
//! lifecycle pack in the request (the starter is the user's file `references/dox-root.md`), and a hook that looks at the work tree at the first prompt.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::dox;
use agentd::direct::hooks::{DirectHook, Event, Hooks, StartupCtx};
use agentd::direct::skills::{Settings, Skills};
use common::{fake_engine_dir, git, repo_root};
use direct_support::{BUCKET, Direct, Opts, RUN};
use harness_protocol::direct::{DirectOutcome, DirectSlice, Skill, SkillFile, s3 as keys};
use harness_protocol::runtime::{InvocationResponse, RepoSource};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const DELIVERY: &str = "lifecycle-delivery";
const AUTHOR: &str = "ham harness|harness@harness.invalid";

// ---- the user's starter, as the pack ships it ---------------------------------------------------------------------------------

fn pack_file(skill: &str, rel: &str) -> String {
    std::fs::read_to_string(
        repo_root()
            .join("lifecycle/pack/.agents/skills")
            .join(skill)
            .join(rel),
    )
    .unwrap()
}

/// The lifecycle contract as `ham lifecycle init` writes it into a project (`pack/AGENTS.md`).
fn contract_text() -> String {
    std::fs::read_to_string(repo_root().join("lifecycle/pack/AGENTS.md")).unwrap()
}

fn starter_text() -> String {
    pack_file(DELIVERY, "references/dox-root.md")
}

fn notice_text() -> String {
    pack_file(DELIVERY, "references/dox-notice.md")
}

/// The delivery skill as `ham` sends it, with `root` as the user's copy of the starter.
fn skill_with(root: &str) -> Skill {
    Skill {
        name: DELIVERY.into(),
        files: vec![
            SkillFile::new("SKILL.md", &pack_file(DELIVERY, "SKILL.md")),
            SkillFile::new("references/dox-notice.md", &notice_text()),
            SkillFile::new("references/dox-root.md", root),
        ],
    }
}

fn delivery_skill() -> Skill {
    skill_with(&starter_text())
}

/// One of the other six skills of the pack: a run that has these and not the delivery skill has no starter.
fn loop_skill() -> Skill {
    Skill {
        name: "lifecycle-loop".into(),
        files: vec![SkillFile::new(
            "SKILL.md",
            &pack_file("lifecycle-loop", "SKILL.md"),
        )],
    }
}

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = direct_support::sha(&bytes);
    d.request_json = bytes;
}

fn with_skills(d: &mut Direct, skills: &[Skill]) {
    with_members(d, json!({ "skills": skills }));
}

/// Replaces the repository the run restores: `edit` changes the source repository, the change is committed and bundled again. Returns the head.
fn rebundle(d: &Direct, edit: impl FnOnce(&Path)) -> String {
    let repo = d.dir.join("python-calc");
    edit(&repo);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "the test's repository"]);
    let bundle = d.dir.join("repo2.bundle");
    git(
        &repo,
        &["bundle", "create", "-q", bundle.to_str().unwrap(), "--all"],
    );
    d.aws
        .put(&keys::repo_bundle_key(RUN), std::fs::read(&bundle).unwrap());
    git(&repo, &["rev-parse", "HEAD"])
}

fn original_head(d: &Direct) -> String {
    git(&d.dir.join("python-calc"), &["rev-parse", "HEAD"])
}

// ---- what the engine finds at its first prompt ----------------------------------------------------------------------------------

/// What the work tree held when the first `session/prompt` was written, and what the engine's own listing of the rules said then.
#[derive(Default)]
struct Seen {
    root: Option<String>,
    notice: Option<String>,
    head: String,
    listing: Value,
    /// Where `AGENTS.md` points, when it is a link, and the text of `src/calc.py` (the file a test links to).
    link: Option<PathBuf>,
    calc: Option<String>,
}

#[derive(Default)]
struct Shared {
    repo: Option<PathBuf>,
    config: Option<PathBuf>,
    seen: Option<Seen>,
}

struct Probe(Arc<Mutex<Shared>>);

impl DirectHook for Probe {
    fn name(&self) -> &'static str {
        "b766-probe"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut s = self.0.lock().unwrap();
            s.repo = Some(ctx.repo.to_path_buf());
            s.config = Some(ctx.config_dir.to_path_buf());
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
        let config = s.config.clone().unwrap();
        let listing = std::process::Command::new(fake_engine_dir().join("fake-acp-engine"))
            .args(["config", "rules", "--json", "--config"])
            .arg(&config)
            .current_dir(&repo)
            .output()
            .unwrap();
        s.seen = Some(Seen {
            root: std::fs::read_to_string(repo.join("AGENTS.md")).ok(),
            notice: std::fs::read_to_string(repo.join("THIRD-PARTY-NOTICES.md")).ok(),
            head: git(&repo, &["rev-parse", "HEAD"]),
            listing: serde_json::from_slice(&listing.stdout).unwrap_or(Value::Null),
            link: std::fs::read_link(repo.join("AGENTS.md")).ok(),
            calc: std::fs::read_to_string(repo.join("src/calc.py")).ok(),
        });
    }
}

/// Runs the invocation with the skills module and the probe, as the standard list would for what this file looks at.
async fn run_probed(d: &Direct) -> (InvocationResponse, Option<Seen>) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let modules: Vec<Arc<dyn DirectHook>> = vec![
        Arc::new(Skills::new(Settings::from_lookup(|_| None))),
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

fn subjects(wt: &Path) -> Vec<String> {
    git(wt, &["log", "--format=%s"])
        .lines()
        .map(String::from)
        .collect()
}

fn seed_commits(wt: &Path) -> usize {
    subjects(wt)
        .iter()
        .filter(|s| s.as_str() == dox::SEED_SUBJECT)
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

/// The project folder of a delivered bundle, cloned.
fn clone_bundle(d: &Direct, label: &str) -> PathBuf {
    let bundle = d.bundle_file().expect("the run delivered a bundle");
    let dest = d.dir.join(label);
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
    // the bundle holds the run's branch and no HEAD, so nothing is checked out by the clone
    git(&dest, &["checkout", "-q", &format!("harness/{RUN}/direct")]);
    dest
}

// ---- session 1 ---------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b766_the_starter_is_the_root_agents_md_at_the_first_prompt_and_its_commit_is_the_base() {
    let mut d = Direct::start(Opts::default()).await;
    with_skills(&mut d, &[delivery_skill()]);
    let orig = original_head(&d);
    let (_, seen) = run_probed(&d).await;
    let seen = seen.expect("the engine got a first prompt");
    // the trigger: the request carries the starter and the repository has no root file
    assert!(!git(&d.dir.join("python-calc"), &["ls-files"]).contains("AGENTS.md"));
    assert_eq!(seen.root.as_deref(), Some(starter_text().as_str()));
    assert_eq!(seen.notice.as_deref(), Some(notice_text().as_str()));
    let s = d.summary();
    let base = s.git.base_sha.clone().unwrap();
    assert_eq!(
        base, seen.head,
        "the base is the head the first prompt started on"
    );
    let wt = d.worktree();
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%an|%ae|%s|%P", &base]),
        format!("{AUTHOR}|{}|{orig}", dox::SEED_SUBJECT),
        "one commit by the harness on top of the head the repository was at"
    );
    assert_eq!(
        changed(&wt, &orig, &base),
        ["AGENTS.md", "THIRD-PARTY-NOTICES.md"]
    );
    assert_eq!(
        git(&wt, &["show", &format!("{base}:AGENTS.md")]),
        starter_text().trim()
    );
    assert_eq!(
        git(&wt, &["show", &format!("{base}:THIRD-PARTY-NOTICES.md")]),
        notice_text().trim()
    );
    assert_eq!(
        git(&wt, &["status", "--porcelain"]),
        "",
        "the tree, the index and the head agree"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_the_starter_is_not_counted_as_the_runs_own_change() {
    let mut with = Direct::start(Opts::default()).await;
    with_skills(&mut with, &[delivery_skill()]);
    let (_, seen) = run_probed(&with).await;
    assert!(seen.unwrap().root.is_some(), "the trigger: a seed was made");
    let without = Direct::start(Opts::default()).await;
    without.invoke().await;
    let (a, b) = (with.summary(), without.summary());
    assert!(b.git.files_changed >= 1, "the engine changed files");
    assert_eq!(
        a.git.files_changed, b.git.files_changed,
        "the same run, with and without the starter, changes the same number of files"
    );
    let wt = with.worktree();
    let base = a.git.base_sha.unwrap();
    let head = a.git.head_sha.unwrap();
    assert_eq!(
        changed(&wt, &base, &head).len() as u32,
        a.git.files_changed,
        "the count is the diff from the commit after the starter"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_run_that_changes_nothing_still_shows_no_changes_and_delivers_the_root() {
    let mut d = Direct::start(Opts {
        scenario: "end_turn_done",
        ..Opts::default()
    })
    .await;
    with_skills(&mut d, &[delivery_skill()]);
    let (_, seen) = run_probed(&d).await;
    assert!(seen.unwrap().root.is_some(), "the trigger: a seed was made");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedNoChanges, "{s:?}");
    assert_eq!(s.git.files_changed, 0);
    assert_eq!(
        s.git.base_sha, s.git.head_sha,
        "nothing but the starter is in the tree"
    );
    let delivered = clone_bundle(&d, "delivered");
    assert_eq!(
        std::fs::read_to_string(delivered.join("AGENTS.md")).unwrap(),
        starter_text(),
        "the bundle the run delivers holds the root file"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_run_that_ends_early_still_delivers_the_root_agents_md() {
    let mut d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 6,
        ..Opts::default()
    })
    .await;
    with_skills(&mut d, &[delivery_skill()]);
    let (_, seen) = run_probed(&d).await;
    assert!(seen.unwrap().root.is_some(), "the trigger: a seed was made");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedTimeout, "{s:?}");
    let delivered = clone_bundle(&d, "delivered-early");
    assert!(
        std::fs::read_to_string(delivered.join("AGENTS.md"))
            .unwrap()
            .contains("# DOX framework"),
        "the run never answered and the root file is delivered"
    );
    assert!(delivered.join("THIRD-PARTY-NOTICES.md").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_the_rules_the_engine_lists_at_its_first_request_hold_the_dox_rules() {
    let mut d = Direct::start(Opts::default()).await;
    with_skills(&mut d, &[delivery_skill()]);
    let (_, seen) = run_probed(&d).await;
    let rows = seen.unwrap().listing;
    let row = rows
        .as_array()
        .and_then(|r| r.iter().find(|x| x["name"] == "Workspace AGENTS.md"))
        .unwrap_or_else(|| panic!("no workspace row in the engine's rules listing: {rows}"));
    let text = row["instructions"].as_str().unwrap();
    for needle in [
        "# DOX framework",
        "## Read Before Editing",
        "## Update After Editing",
        "## Closeout",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
}

// ---- the cases that keep their behaviour -------------------------------------------------------------------------------------------

/// A run whose root file at the start is `root` (committed in the repository), with the delivery skill in the request. Returns the work tree's first
/// prompt view, the summary's base and the head the repository was at.
async fn run_with_root(root: &str) -> (Direct, Seen, String, String) {
    let mut d = Direct::start(Opts::default()).await;
    with_skills(&mut d, &[delivery_skill()]);
    let head = rebundle(&d, |repo| {
        std::fs::write(repo.join("AGENTS.md"), root).unwrap()
    });
    let (_, seen) = run_probed(&d).await;
    let seen = seen.expect("the engine got a first prompt");
    let base = d.summary().git.base_sha.unwrap();
    (d, seen, base, head)
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_root_that_holds_the_dox_framework_is_kept_and_nothing_is_seeded() {
    let mine = format!("{}\nBuild: `make`\n", starter_text());
    let (d, seen, base, head) = run_with_root(&mine).await;
    assert_eq!(seen.root.as_deref(), Some(mine.as_str()), "kept as it was");
    assert_eq!(base, head, "no commit of the harness came before the base");
    assert_eq!(seed_commits(&d.worktree()), 0);
    assert_eq!(seen.notice, None, "the notice is the delivery skill's step");
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_lifecycle_root_that_already_holds_the_framework_is_kept_and_nothing_is_seeded() {
    // the state of a project after a delivery that started from `ham lifecycle init`: the contract, then the starter below it
    let mine = format!("{}\n{}", contract_text(), starter_text());
    assert!(mine.starts_with("# Development lifecycle"), "the trigger");
    let (d, seen, base, head) = run_with_root(&mine).await;
    assert_eq!(seen.root.as_deref(), Some(mine.as_str()), "kept as it was");
    assert_eq!(base, head, "no commit of the harness came before the base");
    assert_eq!(seed_commits(&d.worktree()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_root_with_the_users_own_text_is_not_touched() {
    let own = "Our rules: keep functions short.\n";
    let (d, seen, base, head) = run_with_root(own).await;
    assert_eq!(seen.root.as_deref(), Some(own));
    assert_eq!(base, head);
    assert_eq!(seed_commits(&d.worktree()), 0);
    assert_eq!(seen.notice, None);
}

/// The project's own tools read a blank root as none,
/// so the harness does too: a tracked `AGENTS.md` of no text is replaced by the starter, and the commit shows it as changed.
#[tokio::test(flavor = "multi_thread")]
async fn b766_a_blank_root_counts_as_none_and_gets_the_starter() {
    for blank in ["", "\n\n", "  \t\n", "\u{feff}\r\n"] {
        let (d, seen, base, head) = run_with_root(blank).await;
        assert_eq!(
            seen.root.as_deref(),
            Some(starter_text().as_str()),
            "{blank:?}: the starter, with nothing of the blank file kept"
        );
        assert_eq!(
            seen.notice.as_deref(),
            Some(notice_text().as_str()),
            "{blank:?}"
        );
        assert_ne!(base, head, "{blank:?}: the starter commit is the base");
        let wt = d.worktree();
        assert_eq!(seed_commits(&wt), 1, "{blank:?}");
        assert_eq!(
            git(&wt, &["log", "-1", "--format=%P", &base]),
            head,
            "{blank:?}"
        );
        assert_eq!(
            changed(&wt, &head, &base),
            ["AGENTS.md", "THIRD-PARTY-NOTICES.md"],
            "{blank:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_root_from_lifecycle_init_gets_the_starter_below_its_text() {
    let contract = contract_text();
    assert!(
        contract.starts_with("# Development lifecycle"),
        "the trigger"
    );
    let (d, seen, base, head) = run_with_root(&contract).await;
    let want = format!("{contract}\n{}", starter_text());
    assert_eq!(seen.root.as_deref(), Some(want.as_str()));
    assert_ne!(base, head);
    let wt = d.worktree();
    assert_eq!(seed_commits(&wt), 1);
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%P", &base]),
        head,
        "the starter commit sits on the head the repository was at"
    );
    assert_eq!(
        changed(&wt, &head, &base),
        ["AGENTS.md", "THIRD-PARTY-NOTICES.md"]
    );
    assert!(
        git(&wt, &["diff", "--numstat", &head, &base]).starts_with(|c: char| c.is_ascii_digit()),
        "an addition"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_request_that_bans_the_docs_is_given_no_seed() {
    for prompt in [
        "Add a percent_change(a, b) function with tests. Write no AGENTS.md docs.",
        "Build it without an AGENTS.md.\nAdd percent_change(a, b).",
        "Do not create AGENTS.md. Add percent_change(a, b).",
    ] {
        let mut d = Direct::start(Opts {
            prompt: prompt.into(),
            ..Opts::default()
        })
        .await;
        with_skills(&mut d, &[delivery_skill()]);
        let orig = original_head(&d);
        assert!(!git(&d.dir.join("python-calc"), &["ls-files"]).contains("AGENTS.md"));
        let (_, seen) = run_probed(&d).await;
        let seen = seen.expect("a first prompt");
        assert_eq!(seen.root, None, "{prompt}");
        assert_eq!(seen.notice, None, "{prompt}");
        assert_eq!(d.summary().git.base_sha.as_deref(), Some(orig.as_str()));
        assert_eq!(seed_commits(&d.worktree()), 0, "{prompt}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_starter_whose_first_line_is_dox_off_is_given_no_seed() {
    let mut d = Direct::start(Opts::default()).await;
    with_skills(
        &mut d,
        &[skill_with(
            "DOX off\nThis text is ignored.\n# DOX framework\n",
        )],
    );
    let orig = original_head(&d);
    let (_, seen) = run_probed(&d).await;
    let seen = seen.unwrap();
    assert_eq!(seen.root, None);
    assert_eq!(seen.notice, None);
    assert_eq!(d.summary().git.base_sha.as_deref(), Some(orig.as_str()));
    assert_eq!(seed_commits(&d.worktree()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_run_with_no_skills_or_no_delivery_skill_sends_no_starter() {
    // `--no-skills` and `--no-rules` send no skills at all; a user who keeps only the other six sends no starter either.
    for (label, skills) in [("none", vec![]), ("other", vec![loop_skill()])] {
        let mut d = Direct::start(Opts::default()).await;
        if !skills.is_empty() {
            with_skills(&mut d, &skills);
        }
        let orig = original_head(&d);
        let (_, seen) = run_probed(&d).await;
        let seen = seen.unwrap();
        assert_eq!(seen.root, None, "{label}");
        assert_eq!(d.summary().git.base_sha.as_deref(), Some(orig.as_str()));
        assert_eq!(seed_commits(&d.worktree()), 0, "{label}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_an_existing_notices_file_that_names_dox_is_left_alone_and_one_that_does_not_gets_the_notice_below_it()
 {
    for (label, text, want_added) in [
        (
            "names dox",
            "# Third-party notices\n\nDOX is used here.\n",
            false,
        ),
        ("other", "# Third-party notices\n\nLeft pad, MIT.\n", true),
    ] {
        let mut d = Direct::start(Opts::default()).await;
        with_skills(&mut d, &[delivery_skill()]);
        rebundle(&d, |repo| {
            std::fs::write(repo.join("THIRD-PARTY-NOTICES.md"), text).unwrap()
        });
        let (_, seen) = run_probed(&d).await;
        let seen = seen.unwrap();
        assert!(seen.root.is_some(), "{label}: the root was seeded");
        let got = seen.notice.unwrap();
        if want_added {
            assert_eq!(got, format!("{text}\n{}", notice_text()), "{label}");
        } else {
            assert_eq!(got, text, "{label}");
        }
    }
}

// ---- a root that cannot be seeded safely ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_linked_or_ignored_root_is_given_no_seed_and_the_tree_stays_clean() {
    // a link is never written through; an ignored file is never forced into the history
    for label in ["link", "ignored"] {
        let mut d = Direct::start(Opts::default()).await;
        with_skills(&mut d, &[delivery_skill()]);
        let head = rebundle(&d, |repo| match label {
            "link" => std::os::unix::fs::symlink("src/calc.py", repo.join("AGENTS.md")).unwrap(),
            _ => {
                let ignore = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
                std::fs::write(repo.join(".gitignore"), format!("{ignore}AGENTS.md\n")).unwrap();
            }
        });
        let (resp, seen) = run_probed(&d).await;
        let seen = seen.unwrap();
        assert_ne!(
            d.summary().outcome,
            DirectOutcome::StoppedSetupFailed,
            "{label}: the run goes on: {resp:?}"
        );
        assert_eq!(seen.notice, None, "{label}");
        assert_eq!(d.summary().git.base_sha.as_deref(), Some(head.as_str()));
        assert_eq!(seed_commits(&d.worktree()), 0, "{label}");
        if label == "link" {
            assert_eq!(
                seen.link.as_deref(),
                Some(Path::new("src/calc.py")),
                "still a link"
            );
            assert_eq!(
                seen.calc.as_deref(),
                Some(common::BASE_CALC),
                "the link's target is not touched"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_checkout_the_run_already_has_is_never_seeded() {
    // a second invocation of the same slice on the same machine finds the work tree where the first left it
    let mut d = Direct::start(Opts::default()).await;
    with_skills(&mut d, &[delivery_skill()]);
    let repo = d.dir.join("python-calc");
    let wt = d.worktree();
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git(
        &d.dir,
        &["clone", "-q", repo.to_str().unwrap(), wt.to_str().unwrap()],
    );
    git(
        &wt,
        &["checkout", "-q", "-B", &format!("harness/{RUN}/direct")],
    );
    let (_, seen) = run_probed(&d).await;
    assert_eq!(seen.unwrap().root, None);
    assert_eq!(seed_commits(&wt), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_continue_run_is_not_seeded_and_its_tripwire_still_holds() {
    let mut d = Direct::start(Opts::default()).await;
    with_skills(&mut d, &[delivery_skill()]);
    let head = original_head(&d);
    with_members(
        &mut d,
        json!({"continueFrom": {"runId": "01M4XSOURCERUN00000000001", "headSha": head}}),
    );
    let (_, seen) = run_probed(&d).await;
    let s = d.summary();
    assert_ne!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert_eq!(seen.unwrap().root, None);
    assert_eq!(s.git.base_sha.as_deref(), Some(head.as_str()));
    assert_eq!(seed_commits(&d.worktree()), 0);
}

// ---- the slices of a long run -------------------------------------------------------------------------------------------------

/// Returns the slice, its bundle, the head it left, the starter commit, the head the repository was at before the run (the base the service names:
/// the head of the original bundle) and the original bundle.
struct Slice1 {
    bundle: Vec<u8>,
    head: String,
    seed: String,
    orig: String,
    orig_bundle: Vec<u8>,
}

async fn first_slice(scenario: &'static str, with_starter: bool) -> Slice1 {
    let mut d = Direct::start(Opts {
        scenario,
        ..Opts::default()
    })
    .await;
    if with_starter {
        with_skills(&mut d, &[delivery_skill()]);
    }
    let orig = original_head(&d);
    let (_, _) = run_probed(&d).await;
    let s = d.summary();
    Slice1 {
        bundle: d.aws.get(&keys::bundle_key(RUN)).expect("slice 1 bundled"),
        head: s.git.head_sha.clone().unwrap(),
        seed: s.git.base_sha.clone().unwrap(),
        orig,
        orig_bundle: d.aws.get(&keys::repo_bundle_key(RUN)).unwrap(),
    }
}

async fn second_slice(
    opts: Opts,
    one: &Slice1,
    restore_bundle: Vec<u8>,
    restore_head: &str,
) -> Direct {
    let mut d2 = Direct::start(opts).await;
    with_skills(&mut d2, &[delivery_skill()]);
    d2.aws.put(&keys::bundle_key_t(RUN, 1), restore_bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(one.orig.clone()),
        restore_head: Some(restore_head.to_string()),
        sessions_before: Some(1),
        brief: None,
    });
    d2
}

fn summary_of_slice(d: &Direct, turn: u32) -> harness_protocol::direct::DirectSummary {
    serde_json::from_slice(&d.aws.get(&keys::summary_key_t(RUN, turn)).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_second_slice_adds_no_seed_commit_and_loads_the_root_from_the_restored_bundle() {
    let one = first_slice("writes_files", true).await;
    assert_ne!(
        one.seed, one.orig,
        "the trigger: slice 1 made the starter commit"
    );
    let d2 = second_slice(Opts::default(), &one, one.bundle.clone(), &one.head).await;
    let (_, seen) = run_probed(&d2).await;
    let seen = seen.expect("slice 2 got a first prompt");
    assert_eq!(
        seen.root.as_deref(),
        Some(starter_text().as_str()),
        "the root file comes from the restored bundle"
    );
    let wt = d2.worktree();
    assert_eq!(seed_commits(&wt), 1, "slice 2 made no starter commit");
    let s = summary_of_slice(&d2, 2);
    // the service names the head of the original bundle as the run's base; the work of the run is measured from the starter commit
    assert_eq!(s.git.base_sha.as_deref(), Some(one.seed.as_str()));
    let head = s.git.head_sha.clone().unwrap();
    let files = changed(&wt, &one.seed, &head);
    assert_eq!(files.len() as u32, s.git.files_changed, "{files:?}");
    assert!(!files.contains(&"AGENTS.md".to_string()), "{files:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_second_slice_that_did_no_work_shows_no_changes_against_the_starter() {
    let one = first_slice("end_turn_done", true).await;
    assert_ne!(
        one.seed, one.orig,
        "the trigger: slice 1 made the starter commit"
    );
    assert_eq!(
        one.head, one.seed,
        "the trigger: slice 1 left only the starter"
    );
    let d2 = second_slice(
        Opts {
            scenario: "end_turn_done",
            ..Opts::default()
        },
        &one,
        one.bundle.clone(),
        &one.head,
    )
    .await;
    run_probed(&d2).await;
    let s = summary_of_slice(&d2, 2);
    assert_eq!(s.git.base_sha.as_deref(), Some(one.seed.as_str()));
    assert_eq!(s.git.files_changed, 0, "the starter is not the run's work");
    assert_eq!(s.outcome, DirectOutcome::StoppedNoChanges, "{s:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_replay_slice_that_restores_the_original_bundle_makes_the_same_starter_commit_again()
{
    let one = first_slice("end_turn_done", true).await;
    assert_ne!(
        one.seed, one.orig,
        "the trigger: slice 1 made the starter commit"
    );
    let d2 = second_slice(Opts::default(), &one, one.orig_bundle.clone(), &one.orig).await;
    let (_, seen) = run_probed(&d2).await;
    let seen = seen.unwrap();
    assert_eq!(seen.root.as_deref(), Some(starter_text().as_str()));
    let s = summary_of_slice(&d2, 2);
    assert_eq!(
        s.git.base_sha.as_deref(),
        Some(one.seed.as_str()),
        "the same inputs give the same commit"
    );
    assert_eq!(seen.head, one.seed);
    assert_eq!(seed_commits(&d2.worktree()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn b766_a_commit_that_only_looks_like_the_starter_is_not_the_base() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_COMMIT_MESSAGE", dox::SEED_SUBJECT.into())],
        ..Opts::default()
    })
    .await;
    let orig = original_head(&d);
    d.invoke().await;
    let one = Slice1 {
        bundle: d.aws.get(&keys::bundle_key(RUN)).unwrap(),
        head: d.summary().git.head_sha.clone().unwrap(),
        seed: d.summary().git.base_sha.clone().unwrap(),
        orig: orig.clone(),
        orig_bundle: Vec::new(),
    };
    assert!(
        subjects(&d.worktree()).contains(&dox::SEED_SUBJECT.to_string()),
        "the trigger: a commit with that subject is in the history"
    );
    let d2 = second_slice(Opts::default(), &one, one.bundle.clone(), &one.head).await;
    run_probed(&d2).await;
    let s = summary_of_slice(&d2, 2);
    assert_eq!(
        s.git.base_sha.as_deref(),
        Some(orig.as_str()),
        "only the exact commit the harness makes counts"
    );
}

// ---- the pure parts --------------------------------------------------------------------------------------------------------

#[test]
fn b766_a_ban_is_read_with_the_words_of_the_self_check() {
    for yes in [
        "Write no AGENTS.md docs.",
        "Do not write AGENTS.md.",
        "don't create the AGENTS.md",
        "Build it without an AGENTS.md.",
        "The user asked for no AGENTS.md.",
        "No AGENTS.md.",
        "Add the function.\n- no AGENTS.md",
        "Never include a root AGENTS.md",
    ] {
        assert!(dox::bans(yes), "{yes}");
    }
    for no in [
        "Add a percent_change(a, b) function with tests",
        "Update the AGENTS.md build line",
        "The repository has no AGENTS.md yet.",
        "DOX off",
        "Write no docs.",
        "Do not touch the tests.",
        "",
    ] {
        assert!(!dox::bans(no), "{no}");
    }
}

#[test]
fn b766_dox_off_is_read_from_the_first_line_only() {
    for yes in [
        "DOX off\nrest",
        "DOX off",
        "  dox OFF  \nrest",
        "\u{feff}DOX off\r\nrest",
    ] {
        assert!(dox::is_off(yes), "{yes:?}");
    }
    for no in [
        "# DOX framework\nDOX off\n",
        "DOX offline\n",
        "",
        "\nDOX off\n",
    ] {
        assert!(!dox::is_off(no), "{no:?}");
    }
}

#[test]
fn b766_the_starter_is_the_users_file_in_the_delivery_skill() {
    let all = [delivery_skill()];
    let got = dox::starter(&all).expect("a starter");
    assert_eq!(got.root, starter_text());
    assert_eq!(got.notice.as_deref(), Some(notice_text().as_str()));
    assert_eq!(dox::starter(&[]), None);
    assert_eq!(dox::starter(&[loop_skill()]), None);
    // no notice file: the starter alone
    let mut no_notice = delivery_skill();
    no_notice
        .files
        .retain(|f| f.path != "references/dox-notice.md");
    assert_eq!(dox::starter(&[no_notice]).unwrap().notice, None);
    // no starter file
    let mut no_root = delivery_skill();
    no_root.files.retain(|f| f.path != "references/dox-root.md");
    assert_eq!(dox::starter(&[no_root]), None);
    // a blank starter, and a set of skills the shared check refuses
    assert_eq!(dox::starter(&[skill_with("  \n")]), None);
    let mut bad = delivery_skill();
    bad.files[2].sha256 = "0".repeat(64);
    assert_eq!(dox::starter(&[bad]), None);
}
