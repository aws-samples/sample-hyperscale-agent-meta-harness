//! File modes through the turn commit. The engine's editor writes files 0644 and the role policy refuses `chmod`, so the agents staged the Maven wrapper with `git
//! update-index --chmod=+x mvnw` and committed `100755 mvnw`. The rule these tests pin: a path is committed executable when its file on disk is executable or its
//! index entry is 100755.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{
    GitState, Worktree, commit_turn, hide_paths, prepare_local, restore_hidden,
};
use common::{git, scratch};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MVNW: &str = "#!/bin/sh\nexec java -jar .mvn/wrapper/maven-wrapper.jar \"$@\"\n";
const RUN_SH: &str = "#!/bin/sh\necho tests\n";

/// A Maven-shaped repository committed on main: `mvnw` and `tests/run.sh` at 0644 (`100644`).
fn maven_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("maven-app");
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("pom.xml"), "<project/>\n").unwrap();
    std::fs::write(repo.join("mvnw"), MVNW).unwrap();
    std::fs::write(repo.join("tests/run.sh"), RUN_SH).unwrap();
    for p in ["pom.xml", "mvnw", "tests/run.sh"] {
        set_mode(&repo.join(p), 0o644);
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "core.fileMode", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

async fn worktree(label: &str) -> (PathBuf, Worktree, String) {
    let d = scratch(label);
    let src = maven_repo(&d);
    let wt = prepare_local(&d.join("ws"), "run", "foundation-a1", &src)
        .await
        .unwrap();
    let base = git(&wt.repo, &["rev-parse", "HEAD"]);
    (d, wt, base)
}

fn set_mode(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn disk_executable(p: &Path) -> bool {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o100 != 0
}

/// The mode `rev`'s tree records for `path` (`100644`, `100755`, ...).
fn tree_mode(repo: &Path, rev: &str, path: &str) -> String {
    let line = git(repo, &["ls-tree", rev, "--", path]);
    line.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn an_exec_bit_the_agent_staged_and_committed_survives_the_turn_commit() {
    // The live pattern of turns 3 to 6: update-index, then the agent's own commit, nothing else changed.
    let (_d, wt, _base) = worktree("mode-committed").await;
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    git(&wt.repo, &["commit", "-q", "-m", "make mvnw executable"]);
    let agent = git(&wt.repo, &["rev-parse", "HEAD"]);
    assert!(
        !disk_executable(&wt.repo.join("mvnw")),
        "the editor left it 0644"
    );

    let head = commit_turn(&wt, 3).await.unwrap();

    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "100755");
    assert_eq!(
        head, agent,
        "no `harness: turn 3` commit that flips the mode back"
    );
    assert!(disk_executable(&wt.repo.join("mvnw")));
}

#[tokio::test]
async fn an_exec_bit_the_agent_only_staged_is_committed_100755() {
    let (_d, wt, base) = worktree("mode-staged").await;
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);

    let head = commit_turn(&wt, 1).await.unwrap();

    assert_ne!(head, base, "the staged mode is the turn's change");
    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "100755");
    assert_eq!(
        git(&wt.repo, &["log", "-1", "--format=%s"]),
        "harness: turn 1"
    );
}

#[tokio::test]
async fn an_editor_rewrite_that_drops_the_exec_bit_on_disk_keeps_100755() {
    // A tracked 100755 file the editor rewrites through a temporary file comes back 0644 on disk; the
    // index still says 100755, and the content change is the turn's only change.
    let (_d, wt, _base) = worktree("mode-rewrite").await;
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    git(&wt.repo, &["commit", "-q", "-m", "make mvnw executable"]);
    std::fs::write(wt.repo.join("mvnw"), format!("{MVNW}# wrapper 3.3.4\n")).unwrap();
    set_mode(&wt.repo.join("mvnw"), 0o644);

    let head = commit_turn(&wt, 2).await.unwrap();

    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "100755");
    assert_eq!(
        git(&wt.repo, &["show", &format!("{head}:mvnw")]),
        format!("{MVNW}# wrapper 3.3.4").trim_end()
    );
}

#[tokio::test]
async fn a_new_0755_file_is_committed_100755_and_a_plain_file_stays_100644() {
    // Today's behaviour, kept: the disk mode of a file the index does not mark decides.
    let (_d, wt, _base) = worktree("mode-new").await;
    std::fs::write(wt.repo.join("gradlew"), "#!/bin/sh\n").unwrap();
    set_mode(&wt.repo.join("gradlew"), 0o755);
    std::fs::write(wt.repo.join("NOTES.md"), "notes\n").unwrap();
    set_mode(&wt.repo.join("NOTES.md"), 0o644);

    let head = commit_turn(&wt, 1).await.unwrap();

    assert_eq!(tree_mode(&wt.repo, &head, "gradlew"), "100755");
    assert_eq!(tree_mode(&wt.repo, &head, "NOTES.md"), "100644");
    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "100644", "untouched");
    assert_eq!(tree_mode(&wt.repo, &head, "pom.xml"), "100644", "untouched");
    assert!(!disk_executable(&wt.repo.join("pom.xml")));
}

#[tokio::test]
async fn an_agents_git_config_and_update_index_cannot_change_what_the_harness_commits() {
    // AG2: `git config core.fileMode false` and `git update-index --chmod` are commands the engine may run. The
    // turn-start `.git/config` comes back before agentd's git calls (GitState), so the commit records the exec bits
    // the engine staged and the new file's disk mode.
    let (_d, wt, _base) = worktree("mode-config").await;
    let state = GitState::snapshot(&wt).unwrap();
    git(&wt.repo, &["config", "core.fileMode", "false"]);
    git(&wt.repo, &["update-index", "--chmod=+x", "tests/run.sh"]);
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    std::fs::write(wt.repo.join("gradlew"), "#!/bin/sh\n").unwrap();
    set_mode(&wt.repo.join("gradlew"), 0o755);
    git(&wt.repo, &["add", "gradlew"]);
    git(&wt.repo, &["commit", "-q", "-m", "agent"]);

    state.restore(&wt).unwrap();
    let head = commit_turn(&wt, 1).await.unwrap();

    assert_eq!(git(&wt.repo, &["config", "--get", "core.fileMode"]), "true");
    assert_eq!(tree_mode(&wt.repo, &head, "tests/run.sh"), "100755");
    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "100755");
    assert_eq!(tree_mode(&wt.repo, &head, "gradlew"), "100755");
}

#[tokio::test]
async fn symlinks_and_deleted_files_are_left_to_git_add() {
    // Only a regular file gets its exec bit from the index: a staged symlink keeps its link, and a file
    // the agent deleted is committed deleted.
    let (_d, wt, _base) = worktree("mode-other").await;
    std::os::unix::fs::symlink("mvnw", wt.repo.join("mvnw-link")).unwrap();
    git(&wt.repo, &["add", "mvnw-link"]);
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    std::fs::remove_file(wt.repo.join("mvnw")).unwrap();

    let head = commit_turn(&wt, 1).await.unwrap();

    assert_eq!(tree_mode(&wt.repo, &head, "mvnw-link"), "120000");
    assert_eq!(tree_mode(&wt.repo, &head, "mvnw"), "", "deleted");
    assert!(!wt.repo.join("mvnw").exists());
}

#[tokio::test]
async fn a_turn_start_config_with_core_filemode_false_does_not_hide_modes_from_the_commit() {
    // agentd's own git calls run with core.fileMode=true whatever `.git/config` says: a new 0755 file must be
    // committed 100755 (with fileMode=false git records it 100644), and so must a mode change on disk.
    let (_d, wt, _base) = worktree("mode-filemode-false").await;
    git(&wt.repo, &["config", "core.fileMode", "false"]);
    std::fs::write(wt.repo.join("gradlew"), "#!/bin/sh\n").unwrap();
    set_mode(&wt.repo.join("gradlew"), 0o755);
    set_mode(&wt.repo.join("tests/run.sh"), 0o755);

    let head = commit_turn(&wt, 1).await.unwrap();

    assert_eq!(tree_mode(&wt.repo, &head, "gradlew"), "100755");
    assert_eq!(tree_mode(&wt.repo, &head, "tests/run.sh"), "100755");
}

#[tokio::test]
async fn an_exec_bit_staged_on_a_hidden_path_is_not_given_to_the_restored_file() {
    // Hidden paths (repository hooks, plugins, engine configs) are moved aside for the turn and put back as
    // they were; an exec bit staged in the index for one of them never reaches the restored file.
    let d = scratch("mode-hidden");
    let src = maven_repo(&d);
    std::fs::create_dir_all(src.join(".cline/hooks")).unwrap();
    std::fs::write(src.join(".cline/hooks/pre.sh"), "echo hook\n").unwrap();
    set_mode(&src.join(".cline/hooks/pre.sh"), 0o644);
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "hook"]);
    let wt = prepare_local(&d.join("ws"), "run", "foundation-a1", &src)
        .await
        .unwrap();
    let names: std::collections::BTreeSet<String> = [".cline/hooks".to_string()].into();
    let hidden = hide_paths(&wt, &names).unwrap();
    assert!(!wt.repo.join(".cline/hooks/pre.sh").exists());
    // `--chmod` refuses a path whose file is gone; `--cacheinfo` needs no file.
    let blob = git(&wt.repo, &["rev-parse", "HEAD:.cline/hooks/pre.sh"]);
    git(
        &wt.repo,
        &[
            "update-index",
            "--cacheinfo",
            &format!("100755,{blob},.cline/hooks/pre.sh"),
        ],
    );
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    restore_hidden(&wt, hidden).unwrap();

    let head = commit_turn(&wt, 1).await.unwrap();

    assert!(!disk_executable(&wt.repo.join(".cline/hooks/pre.sh")));
    assert_eq!(tree_mode(&wt.repo, &head, ".cline/hooks/pre.sh"), "100644");
    assert_eq!(
        tree_mode(&wt.repo, &head, "mvnw"),
        "100755",
        "the rest still applies"
    );
}

#[tokio::test]
async fn an_index_entry_behind_a_symlinked_directory_never_changes_a_file_outside_the_worktree() {
    // agentd runs as root: a staged path whose parent is a symlink (here to a directory outside the
    // repository) must not let the exec bit follow the link.
    let (d, wt, _base) = worktree("mode-symlink-parent").await;
    let outside = d.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("tool"), "#!/bin/sh\n").unwrap();
    set_mode(&outside.join("tool"), 0o644);
    std::os::unix::fs::symlink(&outside, wt.repo.join("lib")).unwrap();
    let sha = git(
        &wt.repo,
        &["hash-object", "-w", outside.join("tool").to_str().unwrap()],
    );
    git(
        &wt.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100755,{sha},lib/tool"),
        ],
    );

    let head = commit_turn(&wt, 1).await.unwrap();

    assert!(
        !disk_executable(&outside.join("tool")),
        "the link was not followed"
    );
    assert_eq!(
        tree_mode(&wt.repo, &head, "lib"),
        "120000",
        "git add -A records the link itself"
    );
}

#[tokio::test]
async fn a_hard_linked_file_never_gets_the_exec_bit_from_the_index() {
    // agentd runs as root: a file with a second link (here outside the repository) shares its inode, so
    // an exec bit set through the worktree path would change the other file too (GPT-6 Astra review,
    // 2026-10-07). Such a file keeps its disk mode, and the commit records that mode.
    let (d, wt, _base) = worktree("mode-hardlink").await;
    let outside = d.join("outside-tool");
    std::fs::write(&outside, "#!/bin/sh\n").unwrap();
    set_mode(&outside, 0o644);
    std::fs::hard_link(&outside, wt.repo.join("tool")).unwrap();
    git(&wt.repo, &["add", "tool"]);
    git(&wt.repo, &["update-index", "--chmod=+x", "tool"]);

    let head = commit_turn(&wt, 1).await.unwrap();

    assert!(!disk_executable(&outside), "the other link kept its mode");
    assert_eq!(tree_mode(&wt.repo, &head, "tool"), "100644");
}
