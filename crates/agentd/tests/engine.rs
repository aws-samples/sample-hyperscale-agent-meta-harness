//! Engine launch: program resolution, the setpriv uid split, and the root check.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::{LaunchSpec, command_argv, engine_identity, resolve_program, spawn};
use common::scratch;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn spec(argv: &[&str], run_as: Option<(u32, u32)>) -> LaunchSpec {
    let d = scratch("engine");
    LaunchSpec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: d.clone(),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: d.join("stderr.log"),
        run_as,
        sandbox: None,
    }
}

#[test]
fn programs_resolve_on_the_search_path() {
    let path = vec![PathBuf::from("/nonexistent"), PathBuf::from("/usr/bin")];
    assert_eq!(resolve_program("git", &path), PathBuf::from("/usr/bin/git"));
    assert_eq!(
        resolve_program("/opt/x/cline", &path),
        PathBuf::from("/opt/x/cline")
    );
    assert_eq!(
        resolve_program("no-such-engine-binary", &path),
        PathBuf::from("no-such-engine-binary")
    );
}

#[test]
fn a_uid_split_wraps_the_engine_in_setpriv() {
    let argv = command_argv(&spec(&["git", "--version"], Some((1001, 1002)))).unwrap();
    assert_eq!(
        argv,
        vec![
            "/usr/bin/setpriv",
            "--reuid",
            "1001",
            "--regid",
            "1002",
            "--clear-groups",
            "--no-new-privs",
            "--",
            "/usr/bin/git",
            "--version"
        ]
    );
    assert_eq!(
        command_argv(&spec(&["git", "--version"], None)).unwrap(),
        vec!["/usr/bin/git", "--version"]
    );
    assert!(command_argv(&spec(&[], None)).is_err());
    assert!(spawn(&spec(&[], None)).is_err());
}

#[tokio::test]
async fn the_engine_runs_with_a_clean_environment() {
    let s = spec(&["sh", "-c", "env > env.txt"], None);
    let cwd = s.cwd.clone();
    let mut child = spawn(&s).unwrap();
    assert!(child.wait().await.unwrap().success());
    let env = std::fs::read_to_string(cwd.join("env.txt")).unwrap();
    assert!(env.lines().any(|l| l == "PATH=/usr/bin:/bin"));
    assert!(
        !env.lines()
            .any(|l| l.starts_with("CARGO") || l.starts_with("AWS_")),
        "{env}"
    );
}

#[tokio::test]
async fn no_uid_split_without_root_or_a_user() {
    assert_eq!(engine_identity(None).await, Ok(None));
    // The test runs unprivileged, so even an existing user gives no split.
    let me = std::env::var("USER").unwrap_or_else(|_| "root".into());
    assert_eq!(engine_identity(Some(&me)).await, Ok(None));
}
