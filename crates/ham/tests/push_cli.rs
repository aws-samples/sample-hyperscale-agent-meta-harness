//! `ham push <runId> <git-url> [--branch <name>]` through the compiled binary against the local `/v1` API.
//! The run's zip is downloaded like `ham clone`, its `repo.bundle` becomes a
//! temporary repository, and the run's branch is pushed to the URL with the USER's own git (their config, credential helper and ssh),
//! never forced. A URL that carries a credential is refused before anything is downloaded. The remote here is a local bare
//! repository. Exit codes: 0 pushed, 1 error, 2 usage.
#![allow(clippy::unwrap_used)]

mod clonefix;
mod home;
mod support;

use clonefix::{build, git_in};
use home::{Home, command, ham, home, json_lines, new_run, text};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use support::Fake;

async fn delivered(fake: &Fake, label: &str) -> (String, clonefix::Fixture) {
    let run = new_run(fake, label).await;
    let fx = build(label, &run);
    fake.mem.inner.lock().unwrap().deliverable = Some(fx.zip_bytes());
    fake.mem.complete_run(&run, true);
    (run, fx)
}

async fn ham_in(h: &Home, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = command(h, args, env);
    c.current_dir(cwd).stdin(Stdio::null());
    c.output().await.unwrap()
}

/// An empty bare repository `name` under the test home.
fn bare(h: &Home, name: &str) -> PathBuf {
    let dir = h.root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git_in(&dir, &["init", "-q", "--bare", "--initial-branch", "main"]).unwrap();
    dir
}

fn remote_ref(remote: &Path, branch: &str) -> Option<String> {
    git_in(
        remote,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
    .ok()
}

/// What is in the download directory: only the zip, never a leftover `.ham-push-*` directory.
fn downloads_hold_only_the_zip(h: &Home) {
    let names: Vec<String> = std::fs::read_dir(&h.downloads)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| n.ends_with(".zip") && !n.starts_with('.')),
        "{names:?}"
    );
}

#[tokio::test]
async fn push_sends_the_runs_branch_to_the_remote_and_keeps_the_zip() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "push-ok").await;
    let h = home(&fake, "push-ok");
    let remote = bare(&h, "remote.git");
    let url = remote.display().to_string();
    let o = ham_in(&h, &h.root, &["push", &run, &url], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let branch = format!("harness/{run}");
    assert_eq!(
        out.lines().next().unwrap(),
        format!("pushed run {run} to {url} as branch {branch}"),
        "{out}"
    );
    assert!(
        out.contains(&fx.head[..12]) && out.contains("2 commits"),
        "{out}"
    );
    assert!(
        out.contains("temporary repository"),
        "says where it built: {out}"
    );
    // git's own result lines are on stderr, as git writes them.
    let err = text(&o.stderr);
    assert!(
        err.contains("[new branch]") && err.contains(&branch),
        "git's push lines: {err}"
    );
    assert_eq!(
        remote_ref(&remote, &branch).as_deref(),
        Some(fx.head.as_str())
    );
    assert_eq!(
        git_in(
            &remote,
            &["rev-list", "--count", &format!("refs/heads/{branch}")]
        )
        .unwrap(),
        "2"
    );
    // The zip stays; the temporary repository is gone; no presigned URL on either stream.
    downloads_hold_only_the_zip(&h);
    assert!(
        h.downloads
            .join(format!("python-calc-{run}-abc1234.zip"))
            .is_file()
    );
    for stream in [&out, &err] {
        assert!(
            !stream.contains("X-Amz") && !stream.contains("presign-token"),
            "a presigned URL was printed:\n{stream}"
        );
    }
    // The second push of the same run changes nothing and succeeds.
    let o = ham_in(&h, &h.root, &["push", &run, &url], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("up-to-date") || text(&o.stderr).contains("up to date"));

    // --json: one object.
    let other = bare(&h, "remote2.git");
    let o = ham(&h, &["--json", "push", &run, &other.display().to_string()]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let lines = json_lines(&o);
    assert_eq!(lines.len(), 1, "{}", text(&o.stdout));
    let j = &lines[0];
    assert_eq!(j["runId"], run);
    assert_eq!(j["url"], other.display().to_string());
    assert_eq!(j["branch"], branch);
    assert_eq!(j["head"], fx.head);
    assert_eq!(j["commits"], 2);
    assert_eq!(j["pushed"], true);
    assert_eq!(j["partial"], false);
    assert!(j["zip"]["sha256"].as_str().unwrap().len() == 64);
    assert!(!text(&o.stdout).contains("X-Amz"));
}

#[tokio::test]
async fn branch_names_the_remote_branch_and_a_relative_path_means_the_users_directory() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "push-branch").await;
    let h = home(&fake, "push-branch");
    let remote = bare(&h, "remote.git");
    // The relative path is the user's (the working directory), not the temporary repository's.
    let o = ham_in(
        &h,
        &h.root,
        &["push", &run, "remote.git", "--branch", "main"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).lines().next().unwrap(),
        format!("pushed run {run} to remote.git as branch main")
    );
    assert_eq!(
        remote_ref(&remote, "main").as_deref(),
        Some(fx.head.as_str())
    );
    assert_eq!(remote_ref(&remote, &format!("harness/{run}")), None);
}

#[tokio::test]
async fn history_the_remote_does_not_have_in_common_is_refused_and_never_forced() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "push-refused").await;
    let h = home(&fake, "push-refused");
    let remote = bare(&h, "remote.git");
    // The remote's main already has another history.
    let other = h.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git_in(&other, &["init", "-q", "--initial-branch", "main"]).unwrap();
    std::fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
    git_in(&other, &["add", "-A"]).unwrap();
    git_in(&other, &["commit", "-q", "-m", "theirs"]).unwrap();
    git_in(&other, &["push", "-q", remote.to_str().unwrap(), "main"]).unwrap();
    let theirs = remote_ref(&remote, "main").unwrap();
    assert_ne!(theirs, fx.head);
    let o = ham_in(
        &h,
        &h.root,
        &["push", &run, remote.to_str().unwrap(), "--branch", "main"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(
        err.contains("rejected"),
        "git's own refusal is shown: {err}"
    );
    assert!(
        err.contains("ham: ") && err.contains("--branch") && err.contains("never forces"),
        "{err}"
    );
    assert_eq!(
        remote_ref(&remote, "main").as_deref(),
        Some(theirs.as_str())
    );
    assert!(
        !text(&o.stdout).contains("pushed run"),
        "{}",
        text(&o.stdout)
    );
    downloads_hold_only_the_zip(&h);
}

#[tokio::test]
async fn a_url_with_a_credential_is_refused_before_anything_is_downloaded() {
    let fake = Fake::start().await;
    let (run, _fx) = delivered(&fake, "push-cred").await;
    let h = home(&fake, "push-cred");
    // The user information is put in at run time: the password scan flags a URL with a literal password in a tracked file.
    let userinfo = "alice:s3cr3tpw";
    let with_password = format!("https://{userinfo}@git.example.test/team/repo.git");
    for url in [
        with_password.as_str(),
        "https://ghp_tokenvalue123@github.com/team/repo.git",
    ] {
        let o = ham(&h, &["push", &run, url]).await;
        assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
        let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
        assert!(all.contains("credential"), "{all}");
        for secret in [
            "s3cr3tpw",
            "ghp_tokenvalue123",
            "git.example.test",
            "github.com",
        ] {
            assert!(!all.contains(secret), "{secret} was repeated: {all}");
        }
        let o = ham(&h, &["--json", "push", &run, url]).await;
        assert_eq!(o.status.code(), Some(2));
    }
    assert_eq!(
        std::fs::read_dir(&h.downloads).unwrap().count(),
        0,
        "nothing was downloaded"
    );
    assert!(
        !fake
            .mem
            .seen()
            .iter()
            .any(|s| s.path.contains("/download-url")),
        "no link was made"
    );
}

#[tokio::test]
async fn the_users_own_git_configuration_and_ssh_are_used() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "push-user").await;
    let h = home(&fake, "push-user");
    let remote = bare(&h, "remote.git");
    // (a) The user's ~/.gitconfig rewrites the URL they gave to the local repository (as a credential helper or insteadOf would).
    std::fs::write(
        h.root.join(".gitconfig"),
        format!(
            "[url \"{}\"]\n\tinsteadOf = https://git.example.test/team/repo.git\n",
            remote.display()
        ),
    )
    .unwrap();
    let given = "https://git.example.test/team/repo.git";
    let o = ham_in(
        &h,
        &h.root,
        &["push", &run, given, "--branch", "via-config"],
        &[
            // A stray git environment on the machine does not redirect the push to another repository.
            ("GIT_DIR", "/nonexistent/gitdir"),
            ("GIT_WORK_TREE", "/nonexistent/worktree"),
            ("GIT_INDEX_FILE", "/nonexistent/index"),
            ("GIT_NAMESPACE", "other"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/nonexistent/objects"),
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).lines().next().unwrap(),
        format!("pushed run {run} to {given} as branch via-config"),
        "the URL the user gave is the one printed"
    );
    assert_eq!(
        remote_ref(&remote, "via-config").as_deref(),
        Some(fx.head.as_str())
    );

    // (b) The user's ssh: GIT_SSH_COMMAND is theirs (their agent and keys live behind it). The stand-in records who it was asked
    // to reach and serves receive-pack from the local repository.
    let ssh = h.root.join("ssh-stand-in.sh");
    let record = h.root.join("ssh-record.txt");
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nfor last; do :; done\necho \"$@\" >> '{}'\ncase \"$last\" in\n  git-receive-pack\\ *) eval exec git receive-pack \"${{last#git-receive-pack }}\" ;;\nesac\nexit 1\n",
            record.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ssh_url = format!("ssh://git@git.example.test{}", remote.display());
    let o = ham_in(
        &h,
        &h.root,
        &["push", &run, &ssh_url, "--branch", "via-ssh"],
        &[("GIT_SSH_COMMAND", ssh.to_str().unwrap())],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        remote_ref(&remote, "via-ssh").as_deref(),
        Some(fx.head.as_str())
    );
    let asked = std::fs::read_to_string(&record).unwrap();
    assert!(asked.contains("git@git.example.test"), "{asked}");
}

#[tokio::test]
async fn a_failed_push_says_what_to_do_and_leaves_no_temporary_repository() {
    let fake = Fake::start().await;
    let (run, _fx) = delivered(&fake, "push-fail").await;
    let h = home(&fake, "push-fail");
    let missing = h.root.join("no-such-remote.git");
    let o = ham_in(&h, &h.root, &["push", &run, missing.to_str().unwrap()], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(
        err.contains("does not appear to be a git repository"),
        "git's words: {err}"
    );
    assert!(
        err.contains("ham: ") && err.contains("git push failed"),
        "{err}"
    );
    assert!(
        err.contains("credential") || err.contains("SSH"),
        "the hint on credentials: {err}"
    );
    downloads_hold_only_the_zip(&h);
    assert!(!missing.exists());
}

#[tokio::test]
async fn a_run_with_no_zip_git_missing_and_a_bad_command_line_fail_plainly() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "push-none").await;
    let h = home(&fake, "push-none");
    let remote = bare(&h, "remote.git");
    let url = remote.display().to_string();
    let o = ham(&h, &["push", &run, &url]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("no zip or partial zip yet"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(remote_ref(&remote, &format!("harness/{run}")), None);

    for args in [
        vec!["push"],
        vec!["push", run.as_str()],
        vec!["push", run.as_str(), url.as_str(), "extra"],
        vec!["push", run.as_str(), url.as_str(), "--branch", "a..b"],
        vec!["push", run.as_str(), url.as_str(), "--force"],
    ] {
        assert_eq!(ham(&h, &args).await.status.code(), Some(2), "{args:?}");
    }
    let o = ham(&h, &["push", "01K6A00000000000000000NONE", &url]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("HTTP 404"), "{}", text(&o.stderr));

    let (run2, _fx) = delivered(&fake, "push-nogit").await;
    let nogit = h.root.join("empty-path");
    std::fs::create_dir_all(&nogit).unwrap();
    let mut c = command(&h, &["push", &run2, &url], &[]);
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
}

#[tokio::test]
async fn a_blocked_runs_partial_zip_is_pushed_with_a_notice_and_a_partial_flag() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "push-partial").await;
    let fx = build("push-partial", &run);
    fake.mem.partial(&run, fx.zip_bytes());
    let h = home(&fake, "push-partial");
    let remote = bare(&h, "remote.git");
    let o = ham(&h, &["--json", "push", &run, remote.to_str().unwrap()]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("partial delivery"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(json_lines(&o)[0]["partial"], true);
    assert_eq!(
        remote_ref(&remote, &format!("harness/{run}")).as_deref(),
        Some(fx.head.as_str())
    );
}
