//! The checks around agentd's own git calls on the engine's repository: the settings are compared with the
//! bytes of the start of the run right before the call that applies clean filters, and agentd's environment never reaches git. The stand-in
//! program is the same script as in `workspace_redirect.rs`: it marks a file each time git runs it.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{
    GitState, TurnAuthor, WsError, check_git_dir, commit_turn_verified, git_dir_problem,
    prepare_local,
};
use common::{git, python_calc, scratch};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn runs(marker: &Path) -> usize {
    std::fs::read_to_string(marker).map_or(0, |t| t.lines().count())
}

const BY: TurnAuthor<'static> = TurnAuthor {
    role: "direct",
    agent: "direct",
    task: None,
};

fn edit_a_python_file(repo: &Path) {
    std::fs::write(repo.join("tests/test_calc.py"), "def test_z():\n    pass\n").unwrap();
}

#[tokio::test]
async fn a_checkout_that_is_still_its_own_passes_every_check_and_commits() {
    let d = scratch("ws-verify-ok");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run30", "a30", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    assert_eq!(git_dir_problem(&wt.repo), None);
    check_git_dir(&wt.repo).unwrap();
    edit_a_python_file(&wt.repo);
    state.restore(&wt).unwrap();
    state.verify(&wt).unwrap();
    commit_turn_verified(&wt, 1, &BY, &state).await.unwrap();
    assert_eq!(
        git(&wt.repo, &["log", "--format=%s", "-1"]),
        "harness: turn 1"
    );
}

#[tokio::test]
async fn a_setting_that_changes_after_the_restore_stops_the_commit_before_the_filter_runs() {
    let d = scratch("ws-verify-late");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run31", "a31", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    let (script, marker) = recorder(&d);
    state.restore(&wt).unwrap();
    // A process of the engine that outlived it writes the filter after the restore, while agentd walks the index.
    git(
        &wt.repo,
        &["config", "filter.custom.clean", script.to_str().unwrap()],
    );
    std::fs::write(wt.repo.join(".gitattributes"), "*.py filter=custom\n").unwrap();
    edit_a_python_file(&wt.repo);

    let committed = commit_turn_verified(&wt, 1, &BY, &state).await;
    assert!(
        matches!(committed, Err(WsError::Untrusted(_))),
        "{committed:?}"
    );
    assert_eq!(runs(&marker), 0, "the filter ran in agentd's commit");
    assert!(state.verify(&wt).is_err());
}

#[tokio::test]
async fn an_attributes_file_that_appears_after_the_restore_is_seen_too() {
    let d = scratch("ws-verify-attr");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run32", "a32", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    state.restore(&wt).unwrap();
    std::fs::write(wt.repo.join(".git/info/attributes"), "*.py filter=custom\n").unwrap();
    assert!(matches!(state.verify(&wt), Err(WsError::Untrusted(_))));
}

#[tokio::test]
async fn the_problem_names_what_is_wrong_with_dot_git() {
    let d = scratch("ws-problem");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run33", "a33", &src)
        .await
        .unwrap();
    std::fs::write(wt.repo.join(".git/commondir"), "../elsewhere\n").unwrap();
    let why = git_dir_problem(&wt.repo).unwrap();
    assert!(why.contains("commondir"), "{why}");
    std::fs::remove_file(wt.repo.join(".git/commondir")).unwrap();
    assert_eq!(git_dir_problem(&wt.repo), None);
    // A link in place of the directory.
    let moved = wt.repo.join(".gitreal");
    std::fs::rename(wt.repo.join(".git"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, wt.repo.join(".git")).unwrap();
    let why = git_dir_problem(&wt.repo).unwrap();
    assert!(why.contains("link"), "{why}");
    // No directory at all.
    let none = d.join("not-a-repo");
    std::fs::create_dir_all(&none).unwrap();
    assert!(git_dir_problem(&none).is_some());
}

const CHILD: &str = "HAMH_WS_VERIFY_ENV_CHILD";

#[tokio::test]
async fn the_environment_of_agentd_never_configures_its_git_calls() {
    // Git reads settings from the environment (`GIT_CONFIG_COUNT` with its keys and values, `GIT_CONFIG_PARAMETERS`), and agentd's own
    // environment is not agentd's to trust: it is the deployment's. A filter named there must not start in agentd's commit. The test runs itself
    // again with those variables set, since a process cannot change its own environment safely.
    if std::env::var(CHILD).is_ok() {
        let d = scratch("ws-env-child");
        let src = python_calc(&d);
        let wt = prepare_local(&d.join("ws"), "run34", "a34", &src)
            .await
            .unwrap();
        // The attribute that selects the filter is part of the repository; the filter itself is named only by the environment.
        std::fs::write(wt.repo.join(".gitattributes"), "*.py filter=custom\n").unwrap();
        edit_a_python_file(&wt.repo);
        let marker = PathBuf::from(std::env::var("HAMH_WS_VERIFY_ENV_MARKER").unwrap());
        // TRIGGER PRESENT: a git run with this environment does start the filter.
        git(&wt.repo, &["add", "-A"]);
        let before = runs(&marker);
        assert!(before > 0, "the environment does not configure git here");
        agentd::workspace::commit_turn(&wt, 1).await.unwrap();
        assert_eq!(
            runs(&marker),
            before,
            "agentd's environment started the filter in agentd's commit"
        );
        return;
    }
    let d = scratch("ws-env");
    let (script, marker) = recorder(&d);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_environment_of_agentd_never_configures_its_git_calls",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HAMH_WS_VERIFY_ENV_MARKER", &marker)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "filter.custom.clean")
        .env("GIT_CONFIG_VALUE_0", &script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
