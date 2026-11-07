//! `ham push <runId> <git-url> [--branch <name>]`:
//! the parser side. The work is `crates/ham/src/push.rs`. The command refuses a URL that carries a credential (a password, or a
//! token in front of the `@`) before anything is downloaded, and its messages never repeat the URL.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, Invocation, SUBCOMMANDS, parse};
use ham_core::push::{check_branch, check_git_url, default_branch};
use proptest::prelude::*;

fn cmd(args: &[&str]) -> Command {
    parse(args)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

fn fails(args: &[&str]) -> String {
    parse(args).expect_err("must fail").to_string()
}

/// A URL with its user information put in at run time. The password scan
/// flags a URL that carries a literal password in any tracked file, so the tests below never write one.
fn with_userinfo(scheme: &str, userinfo: &str, rest: &str) -> String {
    format!("{scheme}://{userinfo}@{rest}")
}

const PW: &str = "s3cr3tpw";

#[test]
fn push_takes_a_run_a_git_url_and_an_optional_branch() {
    assert!(SUBCOMMANDS.contains(&"push"));
    assert_eq!(
        cmd(&["push", "R1", "git@example.test:me/repo.git"]),
        Command::Push {
            run_id: "R1".into(),
            url: "git@example.test:me/repo.git".into(),
            branch: None,
        }
    );
    assert_eq!(
        cmd(&[
            "push",
            "R1",
            "https://example.test/me/repo.git",
            "--branch",
            "main"
        ]),
        Command::Push {
            run_id: "R1".into(),
            url: "https://example.test/me/repo.git".into(),
            branch: Some("main".into()),
        }
    );
    // --json stands anywhere, also in front of the value of --branch.
    let inv = parse(&[
        "push",
        "--json",
        "R1",
        "/srv/git/repo.git",
        "--branch",
        "main",
    ])
    .unwrap();
    assert!(inv.json);
    assert_eq!(
        inv.to_argv(),
        [
            "push",
            "R1",
            "/srv/git/repo.git",
            "--branch",
            "main",
            "--json"
        ]
    );
    assert_eq!(parse(&inv.to_argv()).unwrap(), inv);
    // `push` is a command name, so it is never a prompt; a prompt that says it is quoted or uses --prompt.
    assert!(matches!(
        cmd(&["--prompt", "push", "--s3-only", "--direct"]),
        Command::StartDirect(_)
    ));
}

#[test]
fn the_branch_defaults_to_the_runs_own_branch() {
    assert_eq!(
        default_branch("01K6A0000000000000000000R1"),
        "harness/01K6A0000000000000000000R1"
    );
}

#[test]
fn push_usage_errors() {
    for (args, needle) in [
        (vec!["push"], "ham push needs <runId>"),
        (vec!["push", "R1"], "ham push needs <git-url>"),
        (vec!["push", "R1", "x.git", "extra"], "unexpected argument"),
        (vec!["push", "R1", "x.git", "--force"], "unknown option"),
        (
            vec!["push", "R1", "x.git", "--branch"],
            "--branch needs a value",
        ),
        (vec!["push", "R1", "x.git", "--branch", "a..b"], "branch"),
        (vec!["push", "R1", "x.git", "--branch", "-x"], "branch"),
        (vec!["push", "R1", "x.git", "--branch", ""], "branch"),
    ] {
        let err = fails(&args);
        assert!(
            err.starts_with("usage:") && err.contains(needle),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn a_url_that_carries_a_credential_is_refused_and_never_repeated() {
    for (url, secret) in [
        (
            with_userinfo("https", &format!("user:{PW}"), "example.test/me/repo.git"),
            PW,
        ),
        (
            "https://ghp_tokenvalue123@github.com/me/repo.git".to_string(),
            "ghp_tokenvalue123",
        ),
        (
            with_userinfo("http", "u:p4ss", "example.test/r.git"),
            "p4ss",
        ),
        (
            with_userinfo("ssh", "git:hunter2", "example.test/me/repo.git"),
            "hunter2",
        ),
        ("git://tok3n@example.test/r.git".to_string(), "tok3n"),
    ] {
        let err = fails(&["push", "R1", &url]);
        assert!(err.starts_with("usage:"), "{err}");
        assert!(err.contains("credential"), "{url}: {err}");
        assert!(
            err.contains("credential helper") || err.contains("SSH"),
            "{err}"
        );
        assert!(
            !err.contains(secret),
            "the message repeats the secret: {err}"
        );
        assert!(
            !err.contains("example.test"),
            "the message repeats the URL: {err}"
        );
        assert!(check_git_url(&url).is_err());
    }
    // The URL given where the run id belongs is not echoed either.
    let bad = with_userinfo("https", &format!("user:{PW}"), "example.test/r.git");
    let err = fails(&["push", &bad, "R1"]);
    assert!(err.starts_with("usage:") && !err.contains(PW), "{err}");
    let err = fails(&["push", "R1", &bad, "--branch"]);
    assert!(!err.contains(PW), "{err}");
    // A third argument is echoed in the usage error, with the part before the @ masked.
    let err = fails(&["push", "R1", "git@example.test:me/r.git", &bad]);
    assert!(err.contains("unexpected argument"), "{err}");
    assert!(!err.contains(PW) && !err.contains("user:"), "{err}");
}

#[test]
fn the_url_forms_git_users_write_are_accepted() {
    for url in [
        "https://github.com/me/repo.git",
        "https://gitlab.example.test:8443/group/sub/repo.git",
        "http://localhost:3000/me/repo.git",
        "ssh://git@example.test/me/repo.git",
        "ssh://git@example.test:2222/me/repo.git",
        "git://example.test/me/repo.git",
        "git@example.test:me/repo.git",
        "example.test:me/repo.git",
        "file:///srv/git/repo.git",
        "/srv/git/repo.git",
        "../remotes/repo.git",
        "./repo.git",
        "remote.git",
    ] {
        assert_eq!(check_git_url(url), Ok(()), "{url}");
    }
}

#[test]
fn urls_that_are_options_commands_or_odd_are_refused() {
    for url in [
        "",
        "-u",
        "--upload-pack=x",
        "ext::helper x",
        "fd::17",
        "ftp://example.test/repo.git",
        "https://example.test/my repo.git",
        "https://example.test/repo.git\n",
        "https://example.test/repo\u{0}.git",
        "https://",
        "https:///repo.git",
    ] {
        assert!(check_git_url(url).is_err(), "{url:?} was accepted");
        let shown = format!("{url:?}");
        // The parser refuses them too (a URL with a space is split by the shell, so it is tried as one argument).
        let err = parse(&["push", "R1", url]).expect_err(&shown).to_string();
        assert!(err.starts_with("usage:"), "{shown}: {err}");
    }
}

#[test]
fn branch_names_follow_git_ref_rules() {
    for ok in [
        "main",
        "feature/x",
        "harness/01K6A0000000000000000000R1",
        "v1.2",
        "a_b-c",
    ] {
        assert_eq!(check_branch(ok), Ok(()), "{ok}");
    }
    for bad in [
        "", "-x", "a..b", "a b", "a~1", "a^", "a:b", "a?b", "a*b", "a[b", "a\\b", "@", "a@{b",
        "a//b", "/a", "a/", "a.", "a.lock", "a/.b", "a/b.lock", ".a", "a\u{7f}", "a\tb",
    ] {
        assert!(check_branch(bad).is_err(), "{bad:?} was accepted");
    }
}

proptest! {
    #[test]
    fn push_argv_round_trips(
        run in "[A-Z0-9]{1,26}",
        url in "(https://[a-z]{1,10}\\.test/[a-z]{1,8}/[a-z]{1,8}\\.git|git@[a-z]{1,10}\\.test:[a-z]{1,8}/[a-z]{1,8}\\.git|/srv/[a-z]{1,8}/[a-z]{1,8}\\.git)",
        branch in proptest::option::of("[a-z][a-z0-9_-]{0,10}(/[a-z0-9_-]{1,10}){0,2}"),
        json in any::<bool>(),
    ) {
        let inv = Invocation { json, command: Command::Push { run_id: run, url, branch } };
        let argv = inv.to_argv();
        let back = parse(&argv).map_err(|e| TestCaseError::fail(format!("{argv:?}: {e}")))?;
        prop_assert_eq!(back, inv);
    }
}
