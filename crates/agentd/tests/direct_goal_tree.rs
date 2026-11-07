// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! The progress fingerprint of the goal loop: a hash of the work tree that says whether a round
//! changed a file. The records of the lifecycle pack (`.ham/lifecycle/`, or the folder `.ham/records` names; `docs/lifecycle/` too, where an
//! older rules file still writes them) and the paths agentd moved aside are left out, and the engine's own
//! repository is never written. Real git in a scratch directory.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::tree::TreeFp;
use common::{git, scratch};
use std::path::{Path, PathBuf};

/// A repository with one committed file, and an agent-data directory beside it.
fn repo(label: &str) -> (PathBuf, PathBuf) {
    let d = scratch(label);
    let repo = d.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let data = d.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    (repo, data)
}

async fn fp(t: &TreeFp) -> String {
    t.fingerprint().await.unwrap()
}

fn write(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[tokio::test]
async fn the_same_tree_has_the_same_fingerprint_and_an_edit_changes_it_even_when_the_file_was_edited_before()
 {
    let (repo, data) = repo("goal-tree-edit");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let first = fp(&t).await;
    assert_eq!(first, fp(&t).await);
    write(&repo, "a.txt", "two\n");
    let second = fp(&t).await;
    assert_ne!(first, second);
    // a file edited again hashes differently even though `git status` would not change
    write(&repo, "a.txt", "three\n");
    assert_ne!(second, fp(&t).await);
    // a new file and a deleted file change it
    let before = fp(&t).await;
    write(&repo, "b.txt", "new\n");
    assert_ne!(before, fp(&t).await);
}

#[tokio::test]
async fn the_records_of_the_lifecycle_pack_do_not_count() {
    let (repo, data) = repo("goal-tree-lifecycle");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, ".ham/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    write(&repo, ".ham/lifecycle/red/01-x.txt", "command: pytest\n");
    assert_eq!(before, fp(&t).await, "the run log changes every round");
    // the records an older rules file still writes under docs/lifecycle do not count either
    write(&repo, "docs/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    assert_eq!(before, fp(&t).await);
    // the notes beside them are work, and so are other docs
    write(&repo, ".ham/research/01-x.md", "notes\n");
    assert_ne!(before, fp(&t).await);
    let after_notes = fp(&t).await;
    write(&repo, "docs/guide.md", "text\n");
    assert_ne!(after_notes, fp(&t).await);
}

#[tokio::test]
async fn the_paths_agentd_moved_aside_do_not_count_whether_written_as_a_directory_or_a_glob() {
    let (repo, data) = repo("goal-tree-hidden");
    let hidden = vec![
        ".fake-acp".to_string(),
        ".cline/hooks/**".to_string(),
        "weird*name".to_string(),
    ];
    let t = TreeFp::open(&data, &repo, &hidden).await.unwrap();
    let before = fp(&t).await;
    write(&repo, ".fake-acp/turns.log", "prompt\n");
    write(&repo, ".cline/hooks/pre.sh", "echo x\n");
    assert_eq!(before, fp(&t).await);
    // a path with pattern characters is not turned into a pathspec: it is simply not excluded
    write(&repo, "weird-name", "x\n");
    assert_ne!(before, fp(&t).await);
}

#[tokio::test]
async fn files_the_repository_ignores_do_not_count() {
    let (repo, data) = repo("goal-tree-ignore");
    write(&repo, ".gitignore", "target/\n");
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "ignore"]);
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, "target/debug/out", "build output\n");
    assert_eq!(before, fp(&t).await);
}

#[tokio::test]
async fn the_engines_repository_is_not_written() {
    let (repo, data) = repo("goal-tree-untouched");
    let index_before = std::fs::read(repo.join(".git/index")).unwrap();
    let objects_before = git(&repo, &["count-objects", "-v"]);
    write(&repo, "b.txt", "new\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let _ = fp(&t).await;
    assert_eq!(
        std::fs::read(repo.join(".git/index")).unwrap(),
        index_before,
        "the repository's own index"
    );
    assert_eq!(
        git(&repo, &["count-objects", "-v"]),
        objects_before,
        "the repository's own object store"
    );
    assert_eq!(git(&repo, &["status", "--short"]), "?? b.txt");
    assert!(
        data.join("goal.git").is_dir(),
        "the work is done in agentd's own repository"
    );
}

#[tokio::test]
async fn a_work_tree_that_is_gone_gives_no_fingerprint_rather_than_a_wrong_one() {
    let (repo, data) = repo("goal-tree-gone");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    assert!(t.fingerprint().await.is_some());
    std::fs::remove_dir_all(&repo).unwrap();
    assert_eq!(t.fingerprint().await, None);
}

#[tokio::test]
async fn a_moved_records_folder_does_not_count_either_and_the_notes_beside_it_do() {
    let (repo, data) = repo("goal-tree-moved");
    write(&repo, ".ham/records", "run-records\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, "run-records/lifecycle/GOAL.md", "SPECIFIC: x\n");
    write(
        &repo,
        "run-records/lifecycle/LOG.md",
        "RUN | pytest | exit 0\n",
    );
    write(
        &repo,
        "run-records/lifecycle/red/01-x.txt",
        "command: pytest\n",
    );
    assert_eq!(
        before,
        fp(&t).await,
        "the card, the log and the red records change every round"
    );
    // research notes and the context index are work, as they are in .ham/research
    write(&repo, "run-records/research/01-x.md", "notes\n");
    assert_ne!(before, fp(&t).await);
}

#[tokio::test]
async fn the_explicit_docs_layout_leaves_docs_lifecycle_out_and_counts_docs_research() {
    let (repo, data) = repo("goal-tree-docs");
    write(&repo, ".ham/records", "docs\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, "docs/lifecycle/GOAL.md", "SPECIFIC: x\n");
    write(&repo, "docs/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    write(&repo, "docs/lifecycle/red/01-x.txt", "command: pytest\n");
    assert_eq!(before, fp(&t).await);
    write(&repo, "docs/research/01-x.md", "notes\n");
    assert_ne!(before, fp(&t).await);
}

#[tokio::test]
async fn the_hidden_folder_named_in_the_locator_is_the_default_folder() {
    let (repo, data) = repo("goal-tree-hidden-named");
    write(&repo, ".ham/records", ".ham\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, ".ham/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    assert_eq!(before, fp(&t).await);
}

#[tokio::test]
async fn a_locator_written_during_the_run_moves_the_exclusion_with_it() {
    let (repo, data) = repo("goal-tree-late");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    write(
        &repo,
        "run-records/lifecycle/LOG.md",
        "RUN | pytest | exit 0\n",
    );
    let counted = fp(&t).await;
    // the records are moved into the folder the locator names: the log no longer counts
    write(&repo, ".ham/records", "run-records\n");
    let with_locator = fp(&t).await;
    assert_ne!(counted, with_locator, "the locator is a file of the work");
    write(
        &repo,
        "run-records/lifecycle/LOG.md",
        "RUN | pytest | exit 1\n",
    );
    assert_eq!(with_locator, fp(&t).await);
}

#[tokio::test]
async fn records_off_excludes_nothing_new_and_the_default_folders_still_do_not_count() {
    let (repo, data) = repo("goal-tree-off");
    write(&repo, ".ham/records", "off\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, ".ham/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    assert_eq!(before, fp(&t).await);
    write(&repo, "docs/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    assert_eq!(before, fp(&t).await);
    write(&repo, "notes/LOG.md", "work\n");
    assert_ne!(before, fp(&t).await);
}

/// COMPAT-CAH-DATA: a repository that keeps its records in the earlier folder does not count them as progress, as one that keeps them in `.ham`.
#[tokio::test]
async fn the_records_in_the_earlier_folder_do_not_count_either() {
    let (repo, data) = repo("goal-tree-earlier");
    write(&repo, ".cah/lifecycle/GOAL.md", "1. tests pass\n");
    let t = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let before = fp(&t).await;
    write(&repo, ".cah/lifecycle/LOG.md", "RUN | pytest | exit 0\n");
    write(&repo, ".cah/lifecycle/red/01-x.txt", "command: pytest\n");
    assert_eq!(before, fp(&t).await, "the run log changes every round");
    // the notes beside them are work
    write(&repo, ".cah/research/01-x.md", "notes\n");
    assert_ne!(before, fp(&t).await);
}
