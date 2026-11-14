//! The periodic snapshot: the work saved so far, taken without touching the engine's repository.
//! A bare repository of its own borrows the repository's objects through alternates; the engine's index, HEAD, refs and object
//! store stay exactly as they were, so the engine's own `git commit` is never blocked and never sees the snapshot.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::input::{ChangedFiles, DigestSources, TreeSource};
use agentd::direct::snapshot::{
    GitSource, MAX_LOG_BYTES, MAX_PROGRESS_BYTES, MAX_STAT_FILES, ScratchRepo, Snapshotter,
    progress_file, snapshot_json,
};
use common::{canary_key_id, git, python_calc, scratch};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Every file under `dir`, relative, sorted: what a snapshot must not add to the engine's `.git`.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

struct RepoState {
    head: String,
    status: String,
    index: Vec<u8>,
    git_files: Vec<String>,
    refs: String,
}

fn state(repo: &Path) -> RepoState {
    RepoState {
        head: git(repo, &["rev-parse", "HEAD"]),
        status: git(repo, &["status", "--porcelain", "--untracked-files=all"]),
        index: std::fs::read(repo.join(".git/index")).unwrap(),
        git_files: files_under(&repo.join(".git")),
        refs: git(repo, &["for-each-ref"]),
    }
}

async fn setup(label: &str) -> (std::path::PathBuf, std::path::PathBuf, String, Snapshotter) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    (dir, repo, base, snap)
}

#[tokio::test]
async fn a_changed_tree_is_saved_and_the_repository_is_left_exactly_as_it_was() {
    let (dir, repo, base, mut snap) = setup("snap-iso").await;
    std::fs::write(repo.join("src/calc.py"), "def add(a, b):\n    return a + b\n\n\ndef percent_change(a, b):\n    return (b - a) / a * 100\n").unwrap();
    std::fs::write(repo.join("src/new.py"), "x = 1\n").unwrap();
    std::fs::remove_file(repo.join("tests/test_calc.py")).unwrap();
    let before = state(&repo);

    let made = snap.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!(made.count, 1);
    assert_eq!(made.base, base);
    assert!(made.bundle.is_file());

    // The engine's repository: HEAD, status, index bytes, refs and every file under .git are as before.
    let after = state(&repo);
    assert_eq!(after.head, before.head);
    assert_eq!(after.status, before.status);
    assert_eq!(after.index, before.index, "the engine's index is untouched");
    assert_eq!(
        after.refs, before.refs,
        "no snapshot ref in the engine's repository"
    );
    assert_eq!(
        after.git_files, before.git_files,
        "no object file was added to the engine's .git"
    );

    // The bundle is complete: a clone holds the three changes, on top of the base, authored by the harness.
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let snap_ref = "refs/remotes/origin/snapshot";
    assert_eq!(git(&clone, &["rev-parse", snap_ref]), made.commit);
    assert_eq!(git(&clone, &["rev-parse", &format!("{snap_ref}^")]), base);
    assert!(git(&clone, &["show", &format!("{snap_ref}:src/calc.py")]).contains("percent_change"));
    assert_eq!(
        git(&clone, &["show", &format!("{snap_ref}:src/new.py")]),
        "x = 1"
    );
    let tree = git(&clone, &["ls-tree", "-r", "--name-only", snap_ref]);
    assert!(
        !tree.contains("tests/test_calc.py"),
        "a deleted file is not in the snapshot: {tree}"
    );
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%an <%ae>|%s", snap_ref]),
        "ham harness <harness@harness.invalid>|harness: snapshot 1"
    );
}

#[tokio::test]
async fn the_snapshot_repository_is_private_and_borrows_the_objects_of_the_engines() {
    let (dir, repo, _, mut snap) = setup("snap-priv").await;
    let git_dir = dir.join("agent-data/snapshot.git");
    assert_eq!(
        std::fs::metadata(&git_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let alternates = std::fs::read_to_string(git_dir.join("objects/info/alternates")).unwrap();
    assert_eq!(
        alternates.trim(),
        repo.join(".git/objects").display().to_string()
    );
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    snap.snapshot().await.unwrap().unwrap();
    // Nothing of the snapshot's objects is in the engine's object store.
    let n = files_under(&repo.join(".git/objects")).len();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    snap.snapshot().await.unwrap().unwrap();
    assert_eq!(files_under(&repo.join(".git/objects")).len(), n);
}

#[tokio::test]
async fn a_tree_that_equals_the_base_or_the_last_snapshot_makes_none() {
    let (_dir, repo, _, mut snap) = setup("snap-idle").await;
    assert_eq!(
        snap.snapshot().await.unwrap(),
        None,
        "nothing changed since the base"
    );
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let first = snap.snapshot().await.unwrap().unwrap();
    assert_eq!(first.count, 1);
    assert_eq!(
        snap.snapshot().await.unwrap(),
        None,
        "nothing changed since the last snapshot"
    );
    std::fs::write(repo.join("a.txt"), "a2\n").unwrap();
    let second = snap.snapshot().await.unwrap().unwrap();
    assert_eq!(second.count, 2);
    assert_ne!(second.commit, first.commit);
}

#[tokio::test]
async fn ignored_files_stay_out_of_the_snapshot_and_the_repository_s_own_excludes_apply() {
    let (dir, repo, _, mut snap) = setup("snap-ignore").await;
    std::fs::write(repo.join(".gitignore"), "build/\n").unwrap();
    std::fs::create_dir_all(repo.join("build")).unwrap();
    std::fs::write(repo.join("build/out.bin"), "x").unwrap();
    // The exclude file changes after the snapshotter was made: it is read again for each snapshot.
    std::fs::write(repo.join(".git/info/exclude"), "*.log\n").unwrap();
    std::fs::write(repo.join("run.log"), "log\n").unwrap();
    std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
    let made = snap.snapshot().await.unwrap().unwrap();
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let tree = git(
        &clone,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "refs/remotes/origin/snapshot",
        ],
    );
    assert!(
        tree.contains("keep.txt") && tree.contains(".gitignore"),
        "{tree}"
    );
    assert!(
        !tree.contains("build/out.bin") && !tree.contains("run.log"),
        "{tree}"
    );
}

#[tokio::test]
async fn a_snapshot_keeps_the_executable_bit() {
    let (dir, repo, _, mut snap) = setup("snap-mode").await;
    let script = repo.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let made = snap.snapshot().await.unwrap().unwrap();
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let line = git(
        &clone,
        &["ls-tree", "refs/remotes/origin/snapshot", "run.sh"],
    );
    assert!(line.starts_with("100755"), "{line}");
}

#[tokio::test]
async fn the_parent_is_the_repository_head_at_the_time_so_the_engine_s_own_commits_are_kept() {
    let (dir, repo, base, mut snap) = setup("snap-head").await;
    std::fs::write(repo.join("one.txt"), "1\n").unwrap();
    git(&repo, &["add", "one.txt"]);
    git(&repo, &["commit", "-q", "-m", "engine commit"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("two.txt"), "2\n").unwrap();
    let made = snap.snapshot().await.unwrap().unwrap();
    assert_eq!(made.base, base, "the base stays the base of the run");
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let snap_ref = "refs/remotes/origin/snapshot";
    assert_eq!(git(&clone, &["rev-parse", &format!("{snap_ref}^")]), head);
    let tree = git(&clone, &["ls-tree", "-r", "--name-only", snap_ref]);
    assert!(
        tree.contains("one.txt") && tree.contains("two.txt"),
        "{tree}"
    );
}

#[tokio::test]
async fn a_snapshot_does_not_block_the_engine_s_own_git_commit() {
    let (_dir, repo, _, mut snap) = setup("snap-lock").await;
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    snap.snapshot().await.unwrap().unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "after the snapshot"]);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "after the snapshot"
    );
    assert!(!repo.join(".git/index.lock").exists());
}

#[test]
fn snapshot_json_names_the_head_the_base_the_time_and_the_count() {
    let made = agentd::direct::snapshot::SnapshotMade {
        commit: "c".repeat(40),
        base: "b".repeat(40),
        bundle: "/x/snapshot.bundle".into(),
        count: 3,
        final_copy: false,
        bundle_bytes: 0,
    };
    let v: serde_json::Value =
        serde_json::from_str(&snapshot_json(&made, "2026-10-13T18:50:00Z")).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"schemaVersion": 1, "headSha": "c".repeat(40), "baseSha": "b".repeat(40), "at": "2026-10-13T18:50:00Z", "count": 3})
    );
}

#[tokio::test]
async fn a_repository_that_is_not_one_is_an_error_not_a_panic() {
    let dir = scratch("snap-bad");
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let not_a_repo = dir.join("empty");
    std::fs::create_dir_all(&not_a_repo).unwrap();
    assert!(
        Snapshotter::new(&data, &not_a_repo, "0".repeat(40).as_str())
            .await
            .is_err()
    );
}

// ---- review hardening (2026-10-13) ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_tracked_file_that_matches_an_ignore_rule_stays_in_the_snapshot() {
    let dir = scratch("snap-tracked-ignored");
    let repo = python_calc(&dir);
    // Tracked at the base although .gitignore (added later) now ignores it.
    std::fs::write(repo.join("generated.txt"), "tracked at the base\n").unwrap();
    git(&repo, &["add", "generated.txt"]);
    git(&repo, &["commit", "-q", "-m", "track generated"]);
    std::fs::write(repo.join(".gitignore"), "generated.txt\n").unwrap();
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    std::fs::write(repo.join("other.txt"), "o\n").unwrap();
    let made = snap.snapshot().await.unwrap().unwrap();
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let tree = git(
        &clone,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "refs/remotes/origin/snapshot",
        ],
    );
    assert!(
        tree.contains("generated.txt"),
        "a file tracked at the base is not a deletion: {tree}"
    );
    assert!(tree.contains("other.txt"));
}

#[tokio::test]
async fn an_exclude_file_the_engine_swapped_for_a_fifo_does_not_hang_the_snapshot() {
    let (_dir, repo, _, mut snap) = setup("snap-fifo").await;
    let exclude = repo.join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&exclude);
    let st = std::process::Command::new("mkfifo")
        .arg(&exclude)
        .status()
        .unwrap();
    assert!(st.success());
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let made = tokio::time::timeout(std::time::Duration::from_secs(20), snap.snapshot())
        .await
        .expect("the snapshot finished")
        .unwrap();
    assert!(made.is_some());
}

#[tokio::test]
async fn an_exclude_file_that_is_a_symlink_is_not_followed() {
    let (dir, repo, _, mut snap) = setup("snap-exclude-link").await;
    let secret = dir.join("secret-patterns");
    std::fs::write(&secret, "keep.txt\n").unwrap();
    let exclude = repo.join(".git/info/exclude");
    let _ = std::fs::remove_file(&exclude);
    std::os::unix::fs::symlink(&secret, &exclude).unwrap();
    std::fs::write(repo.join("keep.txt"), "k\n").unwrap();
    let made = snap.snapshot().await.unwrap().unwrap();
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            made.bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let tree = git(
        &clone,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "refs/remotes/origin/snapshot",
        ],
    );
    assert!(
        tree.contains("keep.txt"),
        "the pattern behind the link was not read: {tree}"
    );
}

#[tokio::test]
async fn the_scratch_tree_is_content_aware_a_second_edit_of_a_modified_file_changes_it() {
    let dir = scratch("scratch-tree");
    let repo = python_calc(&dir);
    let data = dir.join("probe-data");
    std::fs::create_dir_all(&data).unwrap();
    let scratch_repo = ScratchRepo::new(&data, &repo).await.unwrap();
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let objects_before = files_under(&repo.join(".git/objects"));
    let index_before = std::fs::read(repo.join(".git/index")).unwrap();
    std::fs::write(repo.join("src/calc.py"), "v1\n").unwrap();
    let t1 = scratch_repo.tree().await.unwrap();
    std::fs::write(repo.join("src/calc.py"), "v2\n").unwrap();
    let t2 = scratch_repo.tree().await.unwrap();
    assert_ne!(
        t1, t2,
        "git status says ` M src/calc.py` both times; the tree differs"
    );
    assert_eq!(
        scratch_repo.tree().await.unwrap(),
        t2,
        "stable when nothing changes"
    );
    // The engine's repository is untouched: HEAD, index bytes and object files.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        std::fs::read(repo.join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(files_under(&repo.join(".git/objects")), objects_before);
}

// ---- the git facts of a stop --------------------------------------------------------------------------------
//
// The digest of a stop reads what the work tree holds against the base commit through a private repository of its own
// (`ScratchRepo::facts`), never through `git` in the engine's repository.

async fn facts_setup(label: &str) -> (std::path::PathBuf, std::path::PathBuf, String, ScratchRepo) {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let facts = ScratchRepo::facts(&data, &repo, &base).await.unwrap();
    (dir, repo, base, facts)
}

fn engine_commit(repo: &Path, file: &str, body: &str, subject: &str) -> String {
    std::fs::write(repo.join(file), body).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "-q", "-m", subject]);
    git(repo, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn changed_paths_lists_added_modified_and_deleted_paths_with_the_total_and_leaves_the_repository_alone()
 {
    let (_dir, repo, base, facts) = facts_setup("facts-paths").await;
    std::fs::write(
        repo.join("src/calc.py"),
        "def add(a, b):\n    return b + a\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/new.py"), "x = 1\n").unwrap();
    std::fs::remove_file(repo.join("tests/test_calc.py")).unwrap();
    let before = state(&repo);

    let (paths, total) = facts.changed_paths(&base, 10).await.unwrap();
    assert_eq!(
        paths,
        vec!["src/calc.py", "src/new.py", "tests/test_calc.py"]
    );
    assert_eq!(total, 3);

    let after = state(&repo);
    assert_eq!(after.head, before.head);
    assert_eq!(after.status, before.status);
    assert_eq!(after.index, before.index, "the engine's index is untouched");
    assert_eq!(after.refs, before.refs);
    assert_eq!(
        after.git_files, before.git_files,
        "no object file was added to the engine's .git"
    );
}

#[tokio::test]
async fn changed_paths_caps_the_list_and_still_counts_every_path() {
    let (_dir, repo, base, facts) = facts_setup("facts-cap").await;
    for i in 0..5 {
        std::fs::write(repo.join(format!("f{i}.txt")), "x\n").unwrap();
    }
    let (two, total) = facts.changed_paths(&base, 2).await.unwrap();
    assert_eq!(two, vec!["f0.txt", "f1.txt"]);
    assert_eq!(total, 5);
    let (none, total) = facts.changed_paths(&base, 0).await.unwrap();
    assert!(none.is_empty());
    assert_eq!(total, 5);
}

#[tokio::test]
async fn a_clean_tree_has_no_changed_paths() {
    let (_dir, _repo, base, facts) = facts_setup("facts-clean").await;
    let (paths, total) = facts.changed_paths(&base, 10).await.unwrap();
    assert!(paths.is_empty());
    assert_eq!(total, 0);
}

#[tokio::test]
async fn a_tracked_file_that_an_ignore_rule_matches_is_not_listed_as_deleted() {
    let dir = scratch("facts-ignored");
    let repo = python_calc(&dir);
    std::fs::write(repo.join("keep.log"), "tracked at the base\n").unwrap();
    git(&repo, &["add", "-f", "keep.log"]);
    git(&repo, &["commit", "-q", "-m", "track a log"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join(".git/info/exclude"), "*.log\n").unwrap();
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();

    let seeded = ScratchRepo::facts(&data, &repo, &base).await.unwrap();
    let (paths, total) = seeded.changed_paths(&base, 10).await.unwrap();
    assert!(paths.is_empty(), "seeded with the base: {paths:?}");
    assert_eq!(total, 0);

    // The control: a repository not seeded with the base reads the same file as deleted, which is why `facts` seeds it.
    let unseeded = ScratchRepo::new(&data, &repo).await.unwrap();
    let (paths, total) = unseeded.changed_paths(&base, 10).await.unwrap();
    assert_eq!(paths, vec!["keep.log"]);
    assert_eq!(total, 1);
}

#[tokio::test]
async fn a_path_with_a_newline_or_a_key_id_in_it_is_listed_on_one_line_and_redacted() {
    let (_dir, repo, base, facts) = facts_setup("facts-scrub").await;
    std::fs::write(repo.join("a\nb.txt"), "x\n").unwrap();
    std::fs::write(repo.join(format!("{}.txt", canary_key_id())), "x\n").unwrap();
    let (paths, total) = facts.changed_paths(&base, 10).await.unwrap();
    assert_eq!(total, 2);
    assert!(paths.contains(&"a b.txt".to_string()), "{paths:?}");
    assert!(
        paths.contains(&"<redacted>.txt".to_string()),
        "the key id is redacted: {paths:?}"
    );
}

#[tokio::test]
async fn a_value_that_is_not_an_object_id_never_reaches_git() {
    let (_dir, _repo, base, facts) = facts_setup("facts-ids").await;
    let long = "a".repeat(65);
    for bad in [
        "--output=/x",
        "HEAD",
        "main",
        "abc",
        "",
        long.as_str(),
        "zzzzzzz",
    ] {
        assert!(facts.changed_paths(bad, 5).await.is_err(), "{bad}");
        assert!(facts.tree_of(bad).await.is_err(), "{bad}");
        assert!(facts.log_since(bad, &base, 5).await.is_err(), "{bad}");
        assert!(facts.log_since(&base, bad, 5).await.is_err(), "{bad}");
        assert!(facts.diff_stat(bad, &base).await.is_err(), "{bad}");
        assert!(facts.diff_stat(&base, bad).await.is_err(), "{bad}");
    }
}

#[tokio::test]
async fn tree_of_is_the_tree_id_of_the_commit() {
    let (_dir, repo, base, facts) = facts_setup("facts-treeof").await;
    assert_eq!(
        facts.tree_of(&base).await.unwrap(),
        git(&repo, &["rev-parse", &format!("{base}^{{tree}}")])
    );
}

#[tokio::test]
async fn log_since_lists_the_engines_commits_newest_first_without_touching_the_repository() {
    let (_dir, repo, base, facts) = facts_setup("facts-log").await;
    let c1 = engine_commit(&repo, "feature.py", "v1\n", "add the feature");
    let c2 = engine_commit(&repo, "feature.py", "v2\n", "fix the feature");
    let before = state(&repo);

    let log = facts.log_since(&base, &c2, 20).await.unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[0].ends_with(" fix the feature"), "{log}");
    assert!(lines[1].ends_with(" add the feature"), "{log}");
    assert!(c2.starts_with(lines[0].split(' ').next().unwrap()));
    assert!(c1.starts_with(lines[1].split(' ').next().unwrap()));
    assert!(
        !log.contains(&base[..7]),
        "the base itself is excluded: {log}"
    );

    let after = state(&repo);
    assert_eq!(after.head, before.head);
    assert_eq!(after.refs, before.refs);
    assert_eq!(after.git_files, before.git_files);
}

#[tokio::test]
async fn log_since_honours_the_limit_and_an_empty_range_is_empty() {
    let (_dir, repo, base, facts) = facts_setup("facts-log-limit").await;
    engine_commit(&repo, "a.py", "1\n", "one");
    let head = engine_commit(&repo, "a.py", "2\n", "two");
    let one = facts.log_since(&base, &head, 1).await.unwrap();
    assert_eq!(one.lines().count(), 1, "{one}");
    assert!(one.ends_with(" two"), "{one}");
    assert_eq!(facts.log_since(&head, &head, 5).await.unwrap(), "");
    assert_eq!(facts.log_since(&base, &base, 5).await.unwrap(), "");
}

#[tokio::test]
async fn log_since_scrubs_every_subject() {
    let (_dir, repo, base, facts) = facts_setup("facts-log-scrub").await;
    let head = engine_commit(
        &repo,
        "k.txt",
        "x\n",
        &format!("leaked {} in a subject", canary_key_id()),
    );
    let log = facts.log_since(&base, &head, 5).await.unwrap();
    assert!(log.contains("leaked <redacted> in a subject"), "{log}");
    assert!(!log.contains(&canary_key_id()), "{log}");
}

#[tokio::test]
async fn log_since_never_returns_more_than_its_byte_budget_and_keeps_the_newest_lines() {
    let (_dir, repo, base, facts) = facts_setup("facts-log-bytes").await;
    let mut head = base.clone();
    for i in 0..260 {
        git(
            &repo,
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                &format!("commit {i:03} {}", "w".repeat(280)),
            ],
        );
        head = git(&repo, &["rev-parse", "HEAD"]);
    }
    let log = facts.log_since(&base, &head, 500).await.unwrap();
    assert!(log.len() <= MAX_LOG_BYTES, "{} bytes", log.len());
    assert!(log.lines().next().unwrap().contains("commit 259"), "{log}");
    assert!(log.lines().count() < 260, "the oldest lines were dropped");
    assert!(
        log.lines().all(|l| l.chars().count() <= 300),
        "each line is bounded"
    );
}

#[tokio::test]
async fn diff_stat_names_the_changed_files_and_the_summary_and_is_empty_for_equal_trees() {
    let (_dir, repo, base, facts) = facts_setup("facts-stat").await;
    std::fs::write(
        repo.join("src/calc.py"),
        "def add(a, b):\n    return b + a\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/new.py"), "x = 1\n").unwrap();
    let base_tree = facts.tree_of(&base).await.unwrap();
    let work_tree = facts.tree().await.unwrap();
    let stat = facts.diff_stat(&base_tree, &work_tree).await.unwrap();
    assert!(stat.contains("src/calc.py"), "{stat}");
    assert!(stat.contains("src/new.py"), "{stat}");
    assert!(stat.contains("2 files changed"), "{stat}");
    assert_eq!(facts.diff_stat(&base_tree, &base_tree).await.unwrap(), "");
}

#[tokio::test]
async fn diff_stat_lists_at_most_the_file_cap_then_an_ellipsis_and_still_the_summary() {
    let (_dir, repo, base, facts) = facts_setup("facts-stat-cap").await;
    for i in 0..700 {
        std::fs::write(repo.join(format!("many{i:03}.txt")), "x\n").unwrap();
    }
    let base_tree = facts.tree_of(&base).await.unwrap();
    let work_tree = facts.tree().await.unwrap();
    let stat = facts.diff_stat(&base_tree, &work_tree).await.unwrap();
    let files = stat.lines().filter(|l| l.contains("many")).count();
    assert!(files <= MAX_STAT_FILES, "{files} file lines");
    assert!(files >= MAX_STAT_FILES - 1, "{files} file lines");
    assert!(stat.contains("700 files changed"), "{stat}");
}

#[tokio::test]
async fn the_facts_repository_is_private_and_separate_from_the_probe_and_the_snapshot() {
    let (dir, repo, base, facts) = facts_setup("facts-private").await;
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let objects_before = files_under(&repo.join(".git/objects"));
    let (paths, _) = facts.changed_paths(&base, 5).await.unwrap();
    assert_eq!(paths, vec!["a.txt"], "it read the work tree");
    let data = dir.join("agent-data");
    assert_eq!(
        std::fs::metadata(data.join("facts.git"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!data.join("probe.git").exists());
    assert!(!data.join("snapshot.git").exists());
    assert_eq!(
        files_under(&repo.join(".git/objects")),
        objects_before,
        "the blobs it hashes go to its own object store"
    );
}

#[test]
fn the_progress_file_is_read_from_the_top_of_the_work_tree() {
    let dir = scratch("progress");
    assert_eq!(progress_file(&dir), None, "absent");
    std::fs::write(dir.join("PROGRESS.md"), "step 1 done\nstep 2 next\n").unwrap();
    assert_eq!(
        progress_file(&dir).as_deref(),
        Some("step 1 done\nstep 2 next\n")
    );
}

#[test]
fn the_progress_file_is_cut_at_its_budget_on_a_character_boundary() {
    let dir = scratch("progress-cut");
    std::fs::write(dir.join("PROGRESS.md"), "\u{e9}".repeat(20_000)).unwrap();
    let got = progress_file(&dir).unwrap();
    assert!(got.len() <= MAX_PROGRESS_BYTES, "{} bytes", got.len());
    assert!(got.len() >= MAX_PROGRESS_BYTES - 1, "{} bytes", got.len());
    assert!(got.chars().all(|c| c == '\u{e9}'));
}

#[test]
fn the_progress_file_is_not_read_through_a_link_a_directory_or_when_it_is_huge() {
    let dir = scratch("progress-refuse");
    std::fs::write(dir.join("secret.txt"), "root only\n").unwrap();
    std::os::unix::fs::symlink(dir.join("secret.txt"), dir.join("PROGRESS.md")).unwrap();
    assert_eq!(progress_file(&dir), None, "a link is not followed");
    std::fs::remove_file(dir.join("PROGRESS.md")).unwrap();
    std::fs::create_dir(dir.join("PROGRESS.md")).unwrap();
    assert_eq!(progress_file(&dir), None, "a directory");
    std::fs::remove_dir(dir.join("PROGRESS.md")).unwrap();
    std::fs::write(dir.join("PROGRESS.md"), vec![b'x'; (1 << 20) + 1]).unwrap();
    assert_eq!(progress_file(&dir), None, "over 1 MiB");
}

#[tokio::test]
async fn git_facts_gathers_the_inputs_of_the_digest_in_one_pass() {
    let (_dir, repo, base, facts) = facts_setup("facts-all").await;
    engine_commit(&repo, "feature.py", "v1\n", "add the feature");
    let head = engine_commit(&repo, "feature.py", "v2\n", "fix the feature");
    std::fs::write(repo.join("PROGRESS.md"), "step 2 next\n").unwrap();
    std::fs::write(repo.join("uncommitted.txt"), "u\n").unwrap();
    let before = state(&repo);

    let f = facts.git_facts(&base, &head).await;
    assert_eq!(
        f.changed_files,
        vec!["PROGRESS.md", "feature.py", "uncommitted.txt"]
    );
    assert_eq!(f.files_total, 3);
    let log = f.git_log.expect("two commits since the base");
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.contains("fix the feature") && log.contains("add the feature"));
    let stat = f.diff_stat.expect("the tree differs from the base");
    assert!(stat.contains("3 files changed"), "{stat}");
    assert_eq!(f.progress.as_deref(), Some("step 2 next\n"));
    assert!(f.errors.is_empty(), "{:?}", f.errors);

    let after = state(&repo);
    assert_eq!(after.head, before.head);
    assert_eq!(after.index, before.index);
    assert_eq!(after.git_files, before.git_files);
}

#[tokio::test]
async fn a_part_that_fails_leaves_the_other_facts_intact_and_is_named() {
    let (_dir, repo, base, facts) = facts_setup("facts-part").await;
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let f = facts.git_facts(&base, "not-an-id").await;
    assert_eq!(f.git_log, None);
    assert_eq!(f.changed_files, vec!["a.txt"]);
    assert_eq!(f.files_total, 1);
    assert!(f.diff_stat.unwrap().contains("1 file changed"));
    assert_eq!(f.errors.len(), 1, "{:?}", f.errors);
    assert!(f.errors[0].contains("log"), "{:?}", f.errors);
}

#[tokio::test]
async fn a_tree_that_equals_the_base_has_no_facts_and_no_errors() {
    let (_dir, _repo, base, facts) = facts_setup("facts-none").await;
    let f = facts.git_facts(&base, &base).await;
    assert_eq!(f, agentd::direct::snapshot::GitFacts::default());
}

// ---- the real tree measurer and the git half of the digest sources ------------------------------------------------------------------

#[tokio::test]
async fn the_git_source_answers_the_tree_source_question() {
    let dir = scratch("src-tree");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let src = GitSource::open(&data, &repo, &base).await.unwrap();
    for i in 0..5 {
        std::fs::write(repo.join(format!("f{i}.txt")), "x\n").unwrap();
    }
    assert_eq!(
        src.changed_files(2).await,
        Some(ChangedFiles {
            paths: vec!["f0.txt".into(), "f1.txt".into()],
            total: 5
        })
    );
}

#[tokio::test]
async fn a_tree_that_cannot_be_measured_is_unknown_never_zero() {
    let dir = scratch("src-unknown");
    let repo = python_calc(&dir);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let real = git(&repo, &["rev-parse", "HEAD"]);
    let facts = ScratchRepo::facts(&data, &repo, &real).await.unwrap();
    // A base the repository does not have.
    let src = GitSource::new(facts, "0".repeat(40));
    assert_eq!(src.changed_files(5).await, None);
}

#[tokio::test]
async fn the_git_source_reads_the_engines_head_and_fills_only_the_git_members() {
    let dir = scratch("src-fill");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let src = GitSource::open(&data, &repo, &base).await.unwrap();
    engine_commit(&repo, "feature.py", "v1\n", "add the feature");
    engine_commit(&repo, "feature.py", "v2\n", "fix the feature");
    std::fs::write(repo.join("PROGRESS.md"), "step 2 next\n").unwrap();
    let before = state(&repo);

    let mut sources = DigestSources {
        task: "the task".into(),
        last_message: Some("kept".into()),
        ..DigestSources::default()
    };
    src.facts().await.fill(&mut sources);
    assert_eq!(sources.task, "the task");
    assert_eq!(sources.last_message.as_deref(), Some("kept"));
    assert!(sources.tools.is_empty() && sources.attempts.is_empty());
    assert_eq!(sources.changed_files, vec!["PROGRESS.md", "feature.py"]);
    assert_eq!(sources.files_total, 2);
    let log = sources.git_log.unwrap();
    assert!(
        log.contains("fix the feature") && log.contains("add the feature"),
        "{log}"
    );
    assert!(sources.diff_stat.unwrap().contains("2 files changed"));
    assert_eq!(sources.progress_file.as_deref(), Some("step 2 next\n"));

    let after = state(&repo);
    assert_eq!(after.head, before.head);
    assert_eq!(after.index, before.index);
    assert_eq!(after.git_files, before.git_files);
}

#[tokio::test]
async fn callers_that_overlap_share_the_repository_one_after_the_other() {
    let dir = scratch("src-lock");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let src = GitSource::open(&data, &repo, &base).await.unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let (one, facts, two) = tokio::join!(src.changed_files(9), src.facts(), src.changed_files(9));
    let want = Some(ChangedFiles {
        paths: vec!["a.txt".into()],
        total: 1,
    });
    assert_eq!(one, want);
    assert_eq!(two, want);
    assert_eq!(facts.changed_files, vec!["a.txt"]);
    assert!(facts.errors.is_empty(), "{:?}", facts.errors);
}

#[tokio::test]
async fn a_base_the_repository_does_not_have_is_named_and_the_progress_file_is_still_read() {
    let (_dir, repo, _base, facts) = facts_setup("facts-nobase").await;
    std::fs::write(repo.join("PROGRESS.md"), "still here\n").unwrap();
    let f = facts.git_facts(&"0".repeat(40), &"0".repeat(40)).await;
    assert_eq!(f.progress.as_deref(), Some("still here\n"));
    assert!(f.changed_files.is_empty() && f.diff_stat.is_none());
    assert_eq!(f.files_total, 0);
    assert!(
        f.errors.iter().any(|e| e.starts_with("work tree: ")),
        "{:?}",
        f.errors
    );
}

#[tokio::test]
async fn an_engine_head_that_cannot_be_read_is_named_and_the_other_facts_stay() {
    let (dir, repo, base, facts) = facts_setup("facts-nohead").await;
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let src = GitSource::new(facts, base.clone());
    // Break the engine's repository: no HEAD file, so `git rev-parse HEAD` fails.
    let head_file = repo.join(".git/HEAD");
    std::fs::rename(&head_file, dir.join("HEAD.moved")).unwrap();
    let f = src.facts().await;
    std::fs::rename(dir.join("HEAD.moved"), &head_file).unwrap();
    assert_eq!(f.changed_files, vec!["a.txt"]);
    assert!(f.git_log.is_none());
    assert!(
        f.errors.iter().any(|e| e.starts_with("head: ")),
        "{:?}",
        f.errors
    );
}
