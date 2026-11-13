//! How the launch command of a process of the engine's family is built: through `setpriv` (uid), through the launcher (landlock), never both.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::config::Config;
use agentd::engine::{LaunchSpec, command_argv};
use agentd::sandbox::{Policy, Sandbox};
use common::scratch;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

fn sandbox() -> Sandbox {
    Sandbox {
        launcher: PathBuf::from("/opt/harness/bin/agentd"),
        policy: Arc::new(Policy::for_config(&Config::from_vars(|_| None), None)),
        identity_helper: None,
    }
}

fn spec(argv: &[&str], run_as: Option<(u32, u32)>, sandbox: Option<Sandbox>) -> LaunchSpec {
    let d = scratch("sandbox-argv");
    LaunchSpec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: d.clone(),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: d.join("stderr.log"),
        run_as,
        sandbox,
    }
}

#[test]
fn a_sandboxed_launch_goes_through_the_launcher_with_the_policy_as_json() {
    let sb = sandbox();
    let argv = command_argv(&spec(&["git", "--version"], None, Some(sb.clone()))).unwrap();
    assert_eq!(argv[0], "/opt/harness/bin/agentd");
    assert_eq!(argv[1], "sandbox-exec");
    assert_eq!(argv[2], "--policy-json");
    assert_eq!(argv[3], sb.policy.to_json());
    assert_eq!(argv[4], "--");
    assert_eq!(
        argv[5], "/usr/bin/git",
        "the program is resolved to an absolute path"
    );
    assert_eq!(argv[6], "--version");
    assert_eq!(argv.len(), 7);
}

#[test]
fn the_launch_of_the_unsandboxed_kinds_is_unchanged() {
    assert_eq!(
        command_argv(&spec(&["git", "--version"], None, None)).unwrap(),
        vec!["/usr/bin/git".to_string(), "--version".to_string()]
    );
    let split = command_argv(&spec(&["git"], Some((1001, 1002)), None)).unwrap();
    assert!(split[0].ends_with("setpriv") && split.contains(&"--reuid".to_string()));
}

#[test]
fn a_uid_split_and_a_sandbox_together_are_refused() {
    let err = command_argv(&spec(&["git"], Some((1001, 1002)), Some(sandbox()))).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("both"), "{err}");
}

#[test]
fn a_program_that_does_not_resolve_to_an_absolute_path_is_refused() {
    let err = command_argv(&spec(&["no-such-engine-binary"], None, Some(sandbox()))).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("no-such-engine-binary"), "{err}");
}

#[test]
fn a_launcher_that_is_not_an_absolute_path_is_refused() {
    let mut sb = sandbox();
    sb.launcher = PathBuf::from("agentd");
    let err = command_argv(&spec(&["git"], None, Some(sb))).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("launcher"), "{err}");
}
