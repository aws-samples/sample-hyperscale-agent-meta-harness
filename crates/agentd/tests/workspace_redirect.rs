//! The repository agentd commits in belongs to the engine for the whole run. Whatever the engine wrote there, nothing it chose may start a
//! program when agentd runs `git add -A` after the run. `workspace::GitState` puts `.git/config` and
//! `.git/info/attributes` back, but git can be pointed at another configuration without touching either file: a `.git` that is a file naming the
//! engine's directory, or a `commondir` file inside `.git`. A restore that fails, or never notices, then leaves the engine's clean filter in
//! charge of agentd's own commit.
//!
//! The stand-in below is a script that leaves one line in a marker file each time git runs it and then passes its input through, so a test can
//! count the runs. It is an ordinary script file; no command line or environment carries more than its path.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{GitState, commit_turn, prepare_local};
use common::{git, python_calc, scratch};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A filter program and the file it marks. Returns `(script, marker)`.
fn recorder(dir: &Path) -> (PathBuf, PathBuf) {
    let marker = dir.join("filter-ran");
    let script = dir.join("clean-filter.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho ran >> {}\ncat\n", marker.display()),
    )
    .unwrap();
    let mut perm = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&script, perm).unwrap();
    (script, marker)
}

/// How many times the filter ran.
fn runs(marker: &Path) -> usize {
    std::fs::read_to_string(marker).map_or(0, |t| t.lines().count())
}

/// The engine's edit that the filter acts on: a tracked Python file changes.
fn edit_a_python_file(repo: &Path) {
    std::fs::write(repo.join("tests/test_calc.py"), "def test_z():\n    pass\n").unwrap();
}

#[tokio::test]
async fn a_git_file_that_sends_git_to_the_engines_directory_never_gets_agentds_commit() {
    let d = scratch("ws-gitfile");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run20", "a20", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    let (script, marker) = recorder(&d);
    // The engine moves the repository's directory, leaves a file named `.git` that names it, and defines a clean filter in the moved
    // directory's configuration. It selects the filter with a `.gitattributes` file of the work tree.
    let moved = wt.repo.join(".gitreal");
    std::fs::rename(wt.repo.join(".git"), &moved).unwrap();
    std::fs::write(
        wt.repo.join(".git"),
        format!("gitdir: {}\n", moved.display()),
    )
    .unwrap();
    git(
        &wt.repo,
        &["config", "filter.custom.clean", script.to_str().unwrap()],
    );
    std::fs::write(wt.repo.join(".gitattributes"), "*.py filter=custom\n").unwrap();
    edit_a_python_file(&wt.repo);
    // TRIGGER PRESENT: git run in the checkout reaches the engine's filter through the file.
    git(&wt.repo, &["add", "-A"]);
    let before = runs(&marker);
    assert!(
        before > 0,
        "the redirect does not reach the filter here: the test proves nothing"
    );
    assert!(
        std::fs::symlink_metadata(wt.repo.join(".git"))
            .unwrap()
            .file_type()
            .is_file()
    );

    // The end sequence: the configuration comes back, then agentd commits.
    assert!(
        state.restore(&wt).is_err(),
        "a restore that cannot find the repository's directory is a failure"
    );
    let committed = commit_turn(&wt, 1).await;
    assert!(
        committed.is_err(),
        "agentd committed in a repository the engine redirected: {committed:?}"
    );
    assert_eq!(
        runs(&marker),
        before,
        "the engine's clean filter ran in agentd's own git"
    );
}

#[tokio::test]
async fn a_commondir_file_that_sends_git_to_the_engines_directory_never_gets_agentds_commit() {
    let d = scratch("ws-commondir");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run21", "a21", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    let (script, marker) = recorder(&d);
    // The engine copies the repository's directory, defines the filter in the copy and writes `commondir` in the real one. The real
    // `.git/config` is untouched, so the restore has nothing to put back.
    let copy = wt.repo.join(".gitcopy");
    let status = Command::new("cp")
        .arg("-a")
        .arg(wt.repo.join(".git"))
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    git(
        &wt.repo,
        &[
            "config",
            "--file",
            copy.join("config").to_str().unwrap(),
            "filter.custom.clean",
            script.to_str().unwrap(),
        ],
    );
    std::fs::write(wt.repo.join(".git/commondir"), "../.gitcopy\n").unwrap();
    std::fs::write(wt.repo.join(".gitattributes"), "*.py filter=custom\n").unwrap();
    edit_a_python_file(&wt.repo);
    // TRIGGER PRESENT: git run in the checkout reaches the engine's filter through `commondir`.
    git(&wt.repo, &["add", "-A"]);
    let before = runs(&marker);
    assert!(
        before > 0,
        "commondir does not reach the filter here: the test proves nothing"
    );

    // Nothing in `.git/config` differs from the start of the run; the redirect must still be seen.
    let restored = state.restore(&wt);
    let committed = commit_turn(&wt, 1).await;
    assert!(
        restored.is_err() && committed.is_err(),
        "a commondir file inside .git went unseen: restore {restored:?}, commit {committed:?}"
    );
    assert_eq!(
        runs(&marker),
        before,
        "the engine's clean filter ran in agentd's own git"
    );
}

#[tokio::test]
async fn a_link_in_place_of_the_configuration_file_is_replaced_not_written_through() {
    let d = scratch("ws-config-link");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run22", "a22", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    // A file of agentd's that the engine can name but not write.
    let victim = d.join("victim.txt");
    std::fs::write(&victim, "agentd's own data\n").unwrap();
    let config = wt.repo.join(".git/config");
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(&victim, &config).unwrap();

    state.restore(&wt).unwrap();
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "agentd's own data\n",
        "the restore wrote the repository's configuration into a file the engine linked"
    );
    let now = std::fs::symlink_metadata(&config).unwrap();
    assert!(
        now.file_type().is_file(),
        "the configuration is a file again"
    );
    assert!(
        git(&wt.repo, &["config", "--list", "--local"]).contains("core.bare=false"),
        "the configuration of the start of the run is back"
    );
}

#[tokio::test]
async fn a_link_in_place_of_the_info_directory_is_not_followed_by_the_restore() {
    let d = scratch("ws-info-link");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run23", "a23", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    // The start of the run had no attributes file, so the restore removes whatever it finds. The engine makes `.git/info` a link to a
    // folder that holds a file of the same name.
    let outside = d.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("attributes"), "agentd's own data\n").unwrap();
    std::fs::remove_dir_all(wt.repo.join(".git/info")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.repo.join(".git/info")).unwrap();

    let _ = state.restore(&wt);
    assert!(
        outside.join("attributes").is_file(),
        "the restore removed a file through the link the engine placed"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("attributes")).unwrap(),
        "agentd's own data\n"
    );
}
