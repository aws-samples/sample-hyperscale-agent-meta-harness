//! `ham clone <runId> [dir]` through the compiled binary against the local `/v1` API: the
//! zip is downloaded like `ham pull`, `repo.bundle` is read from it and turned into a git repository with no remote, the tree
//! equals the zip's `src/`, and the user's own git configuration changes nothing. Exit codes: 0 done, 1 error, 2 usage.
#![allow(clippy::unwrap_used)]

mod clonefix;
mod home;
mod support;

use clonefix::{Opts, assert_tree, build, build_opts, git};
use home::{Home, command, ham, home, json_lines, new_run, text};
use serde_json::Value;
use std::process::{Output, Stdio};
use support::Fake;

/// A fake run that completed with `bytes` as its zip.
async fn delivered(
    fake: &Fake,
    label: &str,
    edit: impl FnOnce(&str) -> clonefix::Fixture,
) -> (String, clonefix::Fixture) {
    let run = new_run(fake, label).await;
    let fx = edit(&run);
    fake.mem.inner.lock().unwrap().deliverable = Some(fx.zip_bytes());
    fake.mem.complete_run(&run, true);
    (run, fx)
}

async fn ham_in(h: &Home, cwd: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = command(h, args, env);
    c.current_dir(cwd).stdin(Stdio::null());
    c.output().await.unwrap()
}

#[tokio::test]
async fn clone_makes_a_repository_from_the_delivered_zip_and_keeps_the_zip() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "clone-cli", |r| build("clone-cli", r)).await;
    let h = home(&fake, "clone-cli");
    let dest = h.root.join("out");
    let o = ham(&h, &["clone", &run, dest.to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let first = out.lines().next().unwrap();
    assert_eq!(
        first,
        format!("cloned run {run} into {}", dest.display()),
        "{out}"
    );
    for want in [
        format!("branch harness/{run} at {}", &fx.head[..12]),
        "2 commits".to_string(),
        format!("{} files", fx.items.len()),
        "tree equals the zip's src/".to_string(),
        "no remote is set".to_string(),
    ] {
        assert!(out.contains(&want), "{want:?} missing:\n{out}");
    }
    assert!(!out.contains('\u{1b}'));
    assert_tree(&dest, &fx.items);
    assert_eq!(git(&dest, &["remote"]), "");
    assert_eq!(git(&dest, &["status", "--porcelain"]), "");
    // The zip stays in the download directory (the pull rules), verified.
    let zip = h.downloads.join(format!("python-calc-{run}-abc1234.zip"));
    assert!(zip.is_file(), "{zip:?}");
    assert!(out.contains(&zip.display().to_string()), "{out}");
    assert!(!h.downloads.join(format!("{}.part", zip.display())).exists());

    // --json: one object.
    let dest2 = h.root.join("out2");
    let o = ham(&h, &["--json", "clone", &run, dest2.to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let lines = json_lines(&o);
    assert_eq!(lines.len(), 1, "{}", text(&o.stdout));
    let j = &lines[0];
    assert_eq!(j["runId"], run);
    assert_eq!(j["dir"], dest2.display().to_string());
    assert_eq!(j["branch"], format!("harness/{run}"));
    assert_eq!(j["head"], fx.head);
    assert_eq!(j["commits"], 2);
    assert_eq!(j["files"], fx.items.len());
    assert_eq!(j["partial"], false);
    assert_eq!(j["treeEqualsZip"], true);
    assert_eq!(j["zip"]["path"], zip.display().to_string());
    assert_eq!(j["warnings"], Value::Array(vec![]));
    assert!(j["zip"]["sha256"].as_str().unwrap().len() == 64);
}

#[tokio::test]
async fn clone_without_a_directory_uses_the_repo_name_in_the_working_directory() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "clone-cli-default", |r| {
        build("clone-cli-default", r)
    })
    .await;
    let h = home(&fake, "clone-cli-default");
    let o = ham_in(&h, &h.root, &["clone", &run], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_tree(&h.root.join("python-calc"), &fx.items);
    // The same name again: the directory is not empty, so the second clone stops and says how to pick another.
    let o = ham_in(&h, &h.root, &["clone", &run], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(
        err.contains("not empty") && err.contains(&format!("ham clone {run} <directory>")),
        "{err}"
    );
}

/// An executable shell program at `path` that creates `marker` with a builtin redirect and then runs `tail`.
fn marker_program(path: &std::path::Path, marker: &std::path::Path, tail: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n: > {}\n{tail}", marker.display())).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn the_users_git_configuration_does_not_change_a_clone() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "clone-cli-config", |r| {
        build_opts(
            "clone-cli-config",
            r,
            Opts {
                attributes: true,
                ..Opts::default()
            },
        )
    })
    .await;
    let h = home(&fake, "clone-cli-config");
    let root = h.root.display().to_string();
    // The user's own git configuration (HOME of the child) names every way a program could start inside a clone: a template
    // directory and a global hooks directory with a post-checkout hook each, an fsmonitor program, and a smudge filter that the
    // tracked .gitattributes selects. Each program only leaves a marker file (a shell builtin redirect, no other process); the
    // clone must run none of them and must not copy the template.
    let user_hooks = h.root.join("user-hooks");
    let user_template = h.root.join("user-template");
    std::fs::create_dir_all(&user_hooks).unwrap();
    std::fs::create_dir_all(user_template.join("hooks")).unwrap();
    marker_program(
        &user_hooks.join("post-checkout"),
        &h.root.join("hook-ran"),
        "",
    );
    marker_program(
        &user_template.join("hooks/post-checkout"),
        &h.root.join("template-hook-ran"),
        "",
    );
    marker_program(
        &h.root.join("user-fsmonitor.sh"),
        &h.root.join("fsmonitor-ran"),
        "",
    );
    marker_program(
        &h.root.join("user-smudge.sh"),
        &h.root.join("smudge-ran"),
        "exec cat\n",
    );
    std::fs::write(
        user_template.join("from-template.txt"),
        "from the template\n",
    )
    .unwrap();
    std::fs::write(
        h.root.join(".gitconfig"),
        format!(
            "[init]\n\ttemplateDir = {root}/user-template\n[core]\n\thooksPath = {root}/user-hooks\n\tfsmonitor = {root}/user-fsmonitor.sh\n\tautocrlf = true\n\
             [filter \"lfs\"]\n\tsmudge = {root}/user-smudge.sh\n\tclean = cat\n\trequired = true\n"
        ),
    )
    .unwrap();
    let dest = h.root.join("out");
    let o = ham_in(
        &h,
        &h.root,
        &["clone", &run, dest.to_str().unwrap()],
        &[
            ("GIT_DIR", "/nonexistent/gitdir"),
            ("GIT_WORK_TREE", "/nonexistent/worktree"),
            ("GIT_INDEX_FILE", "/nonexistent/index"),
            ("GIT_NAMESPACE", "other"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/nonexistent/objects"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_0", user_hooks.to_str().unwrap()),
            ("GIT_EXTERNAL_DIFF", "/nonexistent/diff"),
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    for marker in [
        "hook-ran",
        "template-hook-ran",
        "fsmonitor-ran",
        "smudge-ran",
    ] {
        assert!(
            !h.root.join(marker).exists(),
            "{marker}: a program of the user's git configuration ran"
        );
    }
    assert_tree(&dest, &fx.items);
    assert!(
        !dest.join(".git/from-template.txt").exists()
            && !dest.join(".git/hooks/post-checkout").exists(),
        "the user's template directory is not used"
    );
    // The user's configuration is not written into the clone either.
    assert!(clonefix::git_in(&dest, &["config", "--local", "--get", "core.hooksPath"]).is_err());
}

#[tokio::test]
async fn a_gitattributes_conversion_is_a_warning_on_stderr_and_in_the_json() {
    let fake = Fake::start().await;
    let (run, _fx) = delivered(&fake, "clone-cli-crlf", |r| {
        build_opts(
            "clone-cli-crlf",
            r,
            Opts {
                crlf: true,
                ..Opts::default()
            },
        )
    })
    .await;
    let h = home(&fake, "clone-cli-crlf");
    let o = ham(
        &h,
        &[
            "--json",
            "clone",
            &run,
            h.root.join("out").to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(
        err.contains("ham: warning: ") && err.contains("notes.txt"),
        "{err}"
    );
    let j = &json_lines(&o)[0];
    assert_eq!(j["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(j["treeEqualsZip"], true);
}

#[tokio::test]
async fn a_run_with_no_zip_yet_and_a_bad_command_line_fail_plainly() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "clone-cli-none").await;
    let h = home(&fake, "clone-cli-none");
    let o = ham(&h, &["clone", &run, h.root.join("o").to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(err.starts_with("ham: "), "{err}");
    assert!(err.contains("no zip or partial zip yet"), "{err}");
    assert!(!h.root.join("o").exists());
    assert_eq!(ham(&h, &["clone"]).await.status.code(), Some(2));
    assert_eq!(
        ham(&h, &["clone", &run, "a", "b"]).await.status.code(),
        Some(2)
    );
    let o = ham(&h, &["clone", "01K6A00000000000000000NONE", "x"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("HTTP 404"), "{}", text(&o.stderr));
}

#[tokio::test]
async fn a_blocked_runs_partial_zip_is_cloned_with_a_notice_on_stderr_and_a_partial_flag() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "clone-cli-partial").await;
    let fx = build("clone-cli-partial", &run);
    fake.mem.partial(&run, fx.zip_bytes());
    let h = home(&fake, "clone-cli-partial");
    let dest = h.root.join("p");
    let o = ham(&h, &["clone", &run, dest.to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(err.contains("partial delivery"), "{err}");
    let out = text(&o.stdout);
    assert!(out.lines().next().unwrap().contains("PARTIAL"), "{out}");
    assert_tree(&dest, &fx.items);
    let o = ham(
        &h,
        &["--json", "clone", &run, h.root.join("p2").to_str().unwrap()],
    )
    .await;
    assert_eq!(json_lines(&o)[0]["partial"], true);
}

#[tokio::test]
async fn a_destination_that_is_not_empty_stops_before_any_download_and_git_missing_says_what_to_install()
 {
    let fake = Fake::start().await;
    let (run, _fx) = delivered(&fake, "clone-cli-stop", |r| build("clone-cli-stop", r)).await;
    let h = home(&fake, "clone-cli-stop");
    let full = h.root.join("full");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("mine.txt"), "keep").unwrap();
    let o = ham(&h, &["clone", &run, full.to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("not empty"), "{}", text(&o.stderr));
    assert_eq!(
        std::fs::read_to_string(full.join("mine.txt")).unwrap(),
        "keep"
    );
    assert_eq!(
        std::fs::read_dir(&h.downloads).unwrap().count(),
        0,
        "nothing was downloaded"
    );

    let nogit = h.root.join("empty-path");
    std::fs::create_dir_all(&nogit).unwrap();
    let mut c = command(
        &h,
        &["clone", &run, h.root.join("nogit-out").to_str().unwrap()],
        &[],
    );
    c.env("PATH", &nogit).stdin(Stdio::null());
    let o = c.output().await.unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(
        err.contains("git is not available") && err.contains("xcode-select --install"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_dir(&h.downloads).unwrap().count(),
        0,
        "nothing was downloaded"
    );
    assert!(!h.root.join("nogit-out").exists());
}
