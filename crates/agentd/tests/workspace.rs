//! Worktree restore, hidden paths, commit and bundle.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{
    AGENT_AUTHOR, GitState, WsError, commit_turn, exclude_generated, head, hide_paths,
    prepare_from_bundle, prepare_local, restore_hidden, set_repo_identity, write_bundle,
};
use common::{BASE_CALC, git, python_calc, repo_root, scratch};
use harness_core::engine_profile::{hidden_paths_union, load_profiles};
use std::collections::BTreeSet;

fn hidden_set() -> BTreeSet<String> {
    [
        ".cline/hooks",
        ".cline/plugins",
        ".cline/agents",
        "opencode.json",
        ".opencode",
        ".fake-acp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[tokio::test]
async fn a_local_repo_is_copied_under_the_run_and_agent_on_the_harness_branch() {
    let d = scratch("ws-local");
    let src = python_calc(&d);
    let ws = d.join("mnt-workspace");
    let wt = prepare_local(&ws, "run1", "a1", &src).await.unwrap();
    assert_eq!(wt.repo, ws.join("run1/a1/repo"));
    assert_eq!(wt.branch, "harness/run1/a1");
    assert_eq!(
        git(&wt.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "harness/run1/a1"
    );
    assert_eq!(
        git(&wt.repo, &["rev-parse", "HEAD"]),
        git(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        std::fs::read_to_string(wt.repo.join("src/calc.py")).unwrap(),
        BASE_CALC
    );
    // The next turn of the same session reuses the worktree (fast path).
    std::fs::write(wt.repo.join("marker"), "kept").unwrap();
    let again = prepare_local(&ws, "run1", "a1", &src).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(again.repo.join("marker")).unwrap(),
        "kept"
    );
}

#[tokio::test]
async fn a_plain_directory_becomes_a_git_repository() {
    let d = scratch("ws-plain");
    let src = d.join("plain");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("README.md"), "hello\n").unwrap();
    let wt = prepare_local(&d.join("ws"), "run2", "a2", &src)
        .await
        .unwrap();
    assert_eq!(
        git(&wt.repo, &["log", "--format=%s", "-1"]),
        "harness: base"
    );
    assert_eq!(git(&wt.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn hidden_paths_are_absent_during_the_turn_and_restored_after() {
    let d = scratch("ws-hide");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run3", "a3", &src)
        .await
        .unwrap();
    assert!(wt.repo.join(".cline/hooks/pre.sh").exists());
    let hidden = hide_paths(&wt, &hidden_set()).unwrap();
    assert!(!wt.repo.join(".cline/hooks").exists());
    // The engine writes state under a hidden path during the turn; it never reaches the commit.
    std::fs::create_dir_all(wt.repo.join(".fake-acp")).unwrap();
    std::fs::write(wt.repo.join(".fake-acp/turns.log"), "prompt\n").unwrap();
    restore_hidden(&wt, hidden).unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.repo.join(".cline/hooks/pre.sh")).unwrap(),
        "echo repo-supplied hook\n"
    );
    assert!(!wt.repo.join(".fake-acp").exists());
    assert_eq!(git(&wt.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn hidden_paths_that_escape_the_repo_are_refused() {
    let d = scratch("ws-escape");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run4", "a4", &src)
        .await
        .unwrap();
    let bad: BTreeSet<String> = ["../outside".to_string()].into();
    assert!(matches!(hide_paths(&wt, &bad), Err(WsError::Hidden(_))));
}

#[tokio::test]
async fn a_turn_commits_its_changes_and_writes_a_verifiable_bundle() {
    let d = scratch("ws-commit");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run5", "a5", &src)
        .await
        .unwrap();
    let base = git(&wt.repo, &["rev-parse", "HEAD"]);
    // No change: no empty commit.
    assert_eq!(commit_turn(&wt, 1).await.unwrap(), base);
    std::fs::write(
        wt.repo.join("src/calc.py"),
        format!("{BASE_CALC}\n\ndef percent_change(a, b):\n    return (b - a) / a * 100.0\n"),
    )
    .unwrap();
    let sha = commit_turn(&wt, 2).await.unwrap();
    assert_ne!(sha, base);
    assert_eq!(
        git(&wt.repo, &["log", "--format=%s", "-1"]),
        "harness: turn 2"
    );
    let bundle = d.join("out/turns/2/bundle");
    write_bundle(&wt, &bundle).await.unwrap();
    git(&wt.repo, &["bundle", "verify", bundle.to_str().unwrap()]);
    let clone = d.join("clone");
    git(
        &d,
        &[
            "clone",
            "-q",
            "-b",
            "harness/run5/a5",
            bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert!(
        std::fs::read_to_string(clone.join("src/calc.py"))
            .unwrap()
            .contains("percent_change")
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), sha);
}

#[tokio::test]
async fn a_bundle_restores_a_worktree() {
    let d = scratch("ws-bundle");
    let src = python_calc(&d);
    let bundle = d.join("repo.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let wt = prepare_from_bundle(&d.join("ws"), "run6", "a6", &bundle)
        .await
        .unwrap();
    assert_eq!(
        git(&wt.repo, &["rev-parse", "HEAD"]),
        git(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&wt.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "harness/run6/a6"
    );
    let err = prepare_from_bundle(&d.join("ws2"), "run7", "a7", &d.join("missing.bundle"))
        .await
        .unwrap_err();
    assert!(matches!(err, WsError::Git(_)), "{err:?}");
}

/// `git commit` as the engine's shell runs it in a direct session: no author or committer variable, no global or system git
/// configuration, an empty home. Without a repository-local identity git answers "Author identity unknown" (the first live run of
/// stage direct, 2026-10-14: Cline had to commit with a one-off identity).
fn engine_commit(repo: &std::path::Path, home: &std::path::Path) -> std::process::Output {
    std::fs::create_dir_all(home).unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-q", "-m", "by the engine"])
        .current_dir(repo)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // A host whose name is a fully qualified domain name lets git guess an identity; the microVM's is not, and the guess fails
        // there. Taking only a configured identity makes this test the same on every host.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.useConfigOnly")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .unwrap()
}

#[tokio::test]
async fn a_restored_worktree_commits_as_the_agent_without_any_git_configuration() {
    let d = scratch("ws-identity");
    let src = python_calc(&d);
    let bundle = d.join("repo.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let wt = prepare_from_bundle(&d.join("ws"), "run20", "a20", &bundle)
        .await
        .unwrap();
    // Before: the engine cannot commit.
    let before = engine_commit(&wt.repo, &d.join("home"));
    assert!(!before.status.success(), "commit needs an identity");
    assert!(
        String::from_utf8_lossy(&before.stderr).contains("identity unknown"),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    set_repo_identity(&wt).await.unwrap();
    let after = engine_commit(&wt.repo, &d.join("home"));
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    // The convention of the engine's work: "ham agent <agent@harness.invalid>", not the harness's own author.
    let who = format!("{} <{}>", AGENT_AUTHOR.0, AGENT_AUTHOR.1);
    assert_eq!(
        git(&wt.repo, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        format!("{who}|{who}")
    );
    assert_ne!(AGENT_AUTHOR, agentd::workspace::HARNESS_AUTHOR);
}

#[tokio::test]
async fn setting_the_identity_again_changes_nothing_and_keeps_the_agents_own_config() {
    let d = scratch("ws-identity2");
    let src = python_calc(&d);
    let bundle = d.join("repo.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let wt = prepare_from_bundle(&d.join("ws"), "run21", "a21", &bundle)
        .await
        .unwrap();
    set_repo_identity(&wt).await.unwrap();
    git(&wt.repo, &["config", "core.abbrev", "9"]);
    // The next turn of the session finds the worktree and sets the identity again.
    let again = prepare_from_bundle(&d.join("ws"), "run21", "a21", &d.join("ignored"))
        .await
        .unwrap();
    set_repo_identity(&again).await.unwrap();
    assert_eq!(
        git(&wt.repo, &["config", "--local", "--get-all", "user.email"]),
        AGENT_AUTHOR.1
    );
    assert_eq!(
        git(&wt.repo, &["config", "--local", "--get", "core.abbrev"]),
        "9"
    );
}

#[tokio::test]
async fn a_bundle_without_head_starts_from_its_first_ref() {
    let d = scratch("ws-nohead");
    let src = python_calc(&d);
    let bundle = d.join("main-only.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "main"],
    );
    let wt = prepare_from_bundle(&d.join("ws"), "run8", "a8", &bundle)
        .await
        .unwrap();
    assert_eq!(
        git(&wt.repo, &["rev-parse", "HEAD"]),
        git(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&wt.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "harness/run8/a8"
    );
    // Reuse on the next turn.
    let again = prepare_from_bundle(&d.join("ws"), "run8", "a8", &d.join("ignored"))
        .await
        .unwrap();
    assert_eq!(again, wt);
}

#[tokio::test]
async fn the_profiles_hidden_path_patterns_hide_what_they_name() {
    // The profiles write `.cline/hooks/**`; a literal lookup of that pattern hid nothing.
    let d = scratch("ws-profile-hide");
    let src = python_calc(&d);
    for (f, body) in [
        (".cline/plugins/extra.js", "module.exports = {};\n"),
        (".cline/agents/extra.md", "an extra agent note\n"),
        // Cline also discovers hooks in <ws>/.clinerules/hooks; the rules there stay.
        (".clinerules/hooks/PreToolUse", "#!/bin/sh\necho hook\n"),
        (
            ".clinerules/k6-canary.md",
            "Start every Python file with # K6-CANARY\n",
        ),
    ] {
        let path = src.join(f);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "engine config directories"]);
    let wt = prepare_local(&d.join("ws"), "run8", "a8", &src)
        .await
        .unwrap();
    let profiles = load_profiles(&repo_root().join("engine/profiles")).unwrap();
    let hidden = hide_paths(&wt, &hidden_paths_union(&profiles)).unwrap();
    for gone in [
        ".cline/hooks",
        ".cline/plugins",
        ".cline/agents",
        ".clinerules/hooks",
    ] {
        assert!(
            !wt.repo.join(gone).exists(),
            "{gone} still visible to the engine"
        );
    }
    assert!(
        wt.repo.join(".clinerules/k6-canary.md").exists(),
        "repo rules stay visible"
    );
    assert_eq!(
        hidden.quarantined(),
        vec![
            ".cline/agents",
            ".cline/hooks",
            ".cline/plugins",
            ".clinerules/hooks"
        ]
    );
    restore_hidden(&wt, hidden).unwrap();
    assert!(wt.repo.join(".cline/plugins/extra.js").exists());
    assert_eq!(git(&wt.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn hidden_path_patterns_other_than_a_trailing_double_star_are_refused() {
    let d = scratch("ws-glob");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run9", "a9", &src)
        .await
        .unwrap();
    for bad in ["*.js", ".cline/*/x", ".cline/hook?", "a/**/b", "[ab]"] {
        let set: BTreeSet<String> = [bad.to_string()].into();
        assert!(
            matches!(hide_paths(&wt, &set), Err(WsError::Hidden(_))),
            "{bad}"
        );
    }
}

fn diff_names(repo: &std::path::Path, base: &str) -> Vec<String> {
    let out = git(repo, &["diff", "--name-only", base, "HEAD"]);
    out.lines().map(str::to_string).collect()
}

#[tokio::test]
async fn engine_written_git_hooks_and_config_never_run_in_agentds_git_calls() {
    // agentd commits after the engine exits; a hook, a hooksPath, an fsmonitor or a clean filter
    // the engine wrote into the repository must not run then (it could re-add reverted content).
    let d = scratch("ws-git-hooks");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run9", "a9", &src)
        .await
        .unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    let marker = d.join("ran-marker");
    // A stand-in command that appends one line to the marker file each time it runs.
    let record = d.join("record-run.sh");
    std::fs::write(&record, "#!/bin/sh\necho ran >> \"$1\"\n").unwrap();
    let cmd = format!("{} {}", record.display(), marker.display());
    let hook = format!("#!/bin/sh\n{cmd}\n");
    std::fs::write(wt.repo.join(".git/hooks/pre-commit"), &hook).unwrap();
    std::fs::create_dir_all(wt.repo.join("tests/hooks")).unwrap();
    std::fs::write(wt.repo.join("tests/hooks/pre-commit"), &hook).unwrap();
    for p in [
        record.clone(),
        wt.repo.join(".git/hooks/pre-commit"),
        wt.repo.join("tests/hooks/pre-commit"),
    ] {
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    git(&wt.repo, &["config", "core.hooksPath", "tests/hooks"]);
    git(&wt.repo, &["config", "core.fsmonitor", &cmd]);
    git(&wt.repo, &["config", "filter.custom.clean", &cmd]);
    std::fs::write(wt.repo.join(".git/info/attributes"), "*.py filter=custom\n").unwrap();
    std::fs::write(
        wt.repo.join("tests/test_calc.py"),
        "def test_z():\n    pass\n",
    )
    .unwrap();

    state.restore(&wt).unwrap();
    assert!(!git(&wt.repo, &["config", "--list", "--local"]).contains("filter.custom"));
    assert!(!wt.repo.join(".git/info/attributes").exists());
    commit_turn(&wt, 1).await.unwrap();
    assert!(
        !marker.exists(),
        "an engine-written hook, fsmonitor or filter ran"
    );
    assert_eq!(
        git(&wt.repo, &["log", "--format=%s", "-1"]),
        "harness: turn 1"
    );
}

/// Bytecode caches a test run writes: pytest's rewritten test module, a source module's cache,
/// and a bare `.pyc` beside its source.
const BYTECODE: &[&str] = &[
    "tests/__pycache__/test_calc.cpython-312-pytest-9.1.1.pyc",
    "src/__pycache__/calc.cpython-312.pyc",
    "src/legacy.pyc",
];

fn write_bytecode(repo: &std::path::Path) {
    for p in BYTECODE {
        let f = repo.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "bytecode").unwrap();
    }
}

#[tokio::test]
async fn bytecode_caches_never_reach_the_turn_commit_or_fail_the_role() {
    let d = scratch("ws-bytecode");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run11", "a11", &src)
        .await
        .unwrap();
    exclude_generated(&wt).unwrap();
    let base = head(&wt).await.unwrap();
    write_bytecode(&wt.repo);
    std::fs::write(
        wt.repo.join("src/calc.py"),
        format!("{BASE_CALC}\n\ndef sub(a, b):\n    return a - b\n"),
    )
    .unwrap();
    commit_turn(&wt, 1).await.unwrap();
    assert_eq!(diff_names(&wt.repo, &base), vec!["src/calc.py"]);
    for p in BYTECODE {
        assert!(wt.repo.join(p).exists(), "{p} left in place");
    }
}

#[tokio::test]
async fn cargo_build_output_never_reaches_the_turn_commit() {
    // A Rust run in a repository without a.gitignore (a hand-written Cargo.toml) leaves target/ behind: gigabytes of build
    // output that the harness's final commit would add to the delivered branch.
    let d = scratch("ws-cargo-target");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run13", "a13", &src)
        .await
        .unwrap();
    exclude_generated(&wt).unwrap();
    let base = head(&wt).await.unwrap();
    let out = wt.repo.join("target/debug/deps/libx.rlib");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, "rlib").unwrap();
    std::fs::write(
        wt.repo.join("src/calc.py"),
        format!("{BASE_CALC}\n\ndef sub(a, b):\n    return a - b\n"),
    )
    .unwrap();
    commit_turn(&wt, 1).await.unwrap();
    assert_eq!(diff_names(&wt.repo, &base), vec!["src/calc.py"]);
    assert!(out.exists(), "the build output stays in place");
}

#[tokio::test]
async fn a_bundle_worktree_ignores_bytecode_caches_too() {
    let d = scratch("ws-bytecode-bundle");
    let src = python_calc(&d);
    let bundle = d.join("repo.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let wt = prepare_from_bundle(&d.join("ws"), "run12", "a12", &bundle)
        .await
        .unwrap();
    exclude_generated(&wt).unwrap();
    let base = head(&wt).await.unwrap();
    write_bytecode(&wt.repo);
    assert_eq!(
        commit_turn(&wt, 1).await.unwrap(),
        base,
        "no bytecode commit"
    );
    // The next turn of the same session reuses the worktree and keeps one entry per pattern.
    let again = prepare_from_bundle(&d.join("ws"), "run12", "a12", &bundle)
        .await
        .unwrap();
    exclude_generated(&again).unwrap();
    let exclude = std::fs::read_to_string(again.repo.join(".git/info/exclude")).unwrap();
    for entry in ["__pycache__/", "*.py[cod]", "target/"] {
        assert_eq!(
            exclude.lines().filter(|l| *l == entry).count(),
            1,
            "{exclude}"
        );
    }
}
