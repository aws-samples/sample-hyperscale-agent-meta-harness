// COMPAT-CAH: this file runs the client under its earlier name `cah` on purpose.
//! The compiled `ham` started as `cah` (a link to the binary, as `setup.sh` makes it): one notice on standard error, then everything as `ham`
//! does, and the same standard output. The notice goes off with `HAMH_NO_DEPRECATION_NOTICES=1` or `ui.deprecation_notices = false`.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use std::path::PathBuf;
use std::process::Stdio;

const ENDPOINT: &str = "https://ham-alias.invalid/direct";
const NOTICE: &str =
    "cah is the old name of ham and will stop working in a later release. Use ham.";

/// A link named `cah` to the compiled binary, in a folder of its own.
fn alias(h: &home::Home) -> PathBuf {
    let dir = h.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("cah");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ham"), &link).unwrap();
    link
}

async fn run(
    program: impl AsRef<std::ffi::OsStr>,
    h: &home::Home,
    args: &[&str],
    extra: &[(&str, &str)],
) -> std::process::Output {
    home::command_of(program, h, args, extra)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_earlier_name_prints_one_notice_and_the_same_standard_output() {
    let h = home::home_with(ENDPOINT, "alias-same", "ham-test");
    let link = alias(&h);
    for args in [
        vec!["--version"],
        vec!["config", "get", "api_endpoint"],
        vec!["config", "show"],
        vec!["models"],
    ] {
        let new = run(env!("CARGO_BIN_EXE_ham"), &h, &args, &[]).await;
        let old = run(&link, &h, &args, &[]).await;
        assert_eq!(old.status.code(), new.status.code(), "{args:?}");
        assert_eq!(home::text(&old.stdout), home::text(&new.stdout), "{args:?}");
        assert_eq!(
            home::text(&new.stderr),
            "",
            "{args:?}: ham prints no notice"
        );
        assert_eq!(
            home::text(&old.stderr).trim_end(),
            NOTICE,
            "{args:?}: one notice on standard error"
        );
    }
}

#[tokio::test]
async fn the_version_line_is_the_one_of_ham_under_either_name() {
    let h = home::home_with(ENDPOINT, "alias-version", "ham-test");
    let link = alias(&h);
    let o = run(&link, &h, &["--version"], &[]).await;
    let out = home::text(&o.stdout);
    assert!(out.starts_with("ham v"), "{out}");
    assert_eq!(out.lines().count(), 1);
}

#[tokio::test]
async fn the_exit_code_is_the_one_of_ham() {
    let h = home::home_with(ENDPOINT, "alias-exit", "ham-test");
    let link = alias(&h);
    let o = run(&link, &h, &["config", "set", "no_such_key", "1"], &[]).await;
    let new = run(
        env!("CARGO_BIN_EXE_ham"),
        &h,
        &["config", "set", "no_such_key", "1"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), new.status.code());
    assert_ne!(o.status.code(), Some(0));
    assert!(home::text(&o.stderr).contains(NOTICE));
}

#[tokio::test]
async fn the_switch_silences_the_notice() {
    let h = home::home_with(ENDPOINT, "alias-switch", "ham-test");
    let link = alias(&h);
    let o = run(
        &link,
        &h,
        &["config", "get", "api_endpoint"],
        &[("HAMH_NO_DEPRECATION_NOTICES", "1")],
    )
    .await;
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(home::text(&o.stderr), "");
}

#[tokio::test]
async fn the_setting_silences_the_notice() {
    let h = home::home_with(ENDPOINT, "alias-setting", "ham-test");
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{ENDPOINT}\"\nconfigVersion = 1\n[ui]\ndeprecation_notices = false\n"
        ),
    )
    .unwrap();
    let link = alias(&h);
    let o = run(&link, &h, &["config", "get", "api_endpoint"], &[]).await;
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(home::text(&o.stderr), "");
    let o = run(&link, &h, &["--version"], &[]).await;
    assert_eq!(home::text(&o.stderr), "", "also for the version line");
}

#[tokio::test]
async fn a_file_that_is_not_a_link_to_ham_but_is_named_cah_is_the_same_program() {
    // The name is read from argv[0], never from the path the binary was found at: a copy of the program named cah behaves as the link does.
    let h = home::home_with(ENDPOINT, "alias-copy", "ham-test");
    let dir = h.root.join("bin2");
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("cah");
    std::fs::copy(env!("CARGO_BIN_EXE_ham"), &copy).unwrap();
    let o = run(&copy, &h, &["--version"], &[]).await;
    assert_eq!(home::text(&o.stderr).trim_end(), NOTICE);
    assert!(home::text(&o.stdout).starts_with("ham v"));
}

#[tokio::test]
async fn a_name_that_only_contains_cah_is_not_the_earlier_name() {
    let h = home::home_with(ENDPOINT, "alias-other", "ham-test");
    let dir = h.root.join("bin3");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("cahoots");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ham"), &link).unwrap();
    let o = run(&link, &h, &["--version"], &[]).await;
    assert_eq!(home::text(&o.stderr), "");
}

#[test]
fn the_name_the_program_was_started_under_is_its_file_name() {
    use ham::cli::{EARLIER_COMMAND, started_under_earlier_name};
    assert_eq!(EARLIER_COMMAND, "cah");
    assert!(started_under_earlier_name(Some("cah")));
    assert!(started_under_earlier_name(Some("/usr/local/bin/cah")));
    assert!(started_under_earlier_name(Some("./bin/../cah")));
    for other in [
        "ham",
        "/usr/local/bin/ham",
        "cahoots",
        "/cah/ham",
        "cah.exe",
        "",
    ] {
        assert!(!started_under_earlier_name(Some(other)), "{other:?}");
    }
    assert!(!started_under_earlier_name(None));
}
