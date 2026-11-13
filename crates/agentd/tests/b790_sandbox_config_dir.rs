//! The Landlock grant of the engine's own configuration directory. First live OpenCode short-session proof on the Instances runtime:
//! "OpenCode did not start... EACCES: permission denied, mkdir '.../direct/config/opencode'".
//! The sandbox granted every engine's configuration directory read only, which is right for Cline (it writes nothing there; root owns the directory)
//! and wrong for an engine that keeps state in its own configuration directory and finds the harness's files in a root-owned directory beside it.
#![allow(clippy::unwrap_used)]

use agentd::engine::direct::grant_config_dir;
use agentd::sandbox::policy::Policy;
use std::path::Path;

fn empty() -> Policy {
    serde_json::from_str(
        r#"{"version":1,"minAbi":6,"readWrite":[],"readExec":[],"readOnly":[],"devices":[],"private":[],"denyConnect":[],"denyBind":[]}"#,
    )
    .unwrap()
}

fn has(grants: &[agentd::sandbox::policy::Grant], p: &str) -> bool {
    grants.iter().any(|g| g.path == Path::new(p))
}

const CONFIG: &str = "/var/lib/agentd/RUN/direct/config";

#[test]
fn an_engine_with_a_separate_root_may_write_its_own_configuration_directory() {
    let p = grant_config_dir(empty(), "opencode", Path::new(CONFIG));
    assert!(has(&p.read_write, CONFIG), "{p:?}");
    assert!(
        !has(&p.read_only, CONFIG),
        "a directory is granted once: {p:?}"
    );
}

#[test]
fn cline_keeps_its_configuration_directory_read_only() {
    let p = grant_config_dir(empty(), "cline", Path::new(CONFIG));
    assert!(has(&p.read_only, CONFIG), "{p:?}");
    assert!(!has(&p.read_write, CONFIG), "{p:?}");
}

#[test]
fn an_engine_with_no_arm_keeps_the_read_only_grant_it_always_had() {
    let p = grant_config_dir(empty(), "pi", Path::new(CONFIG));
    assert!(has(&p.read_only, CONFIG), "{p:?}");
    assert!(!has(&p.read_write, CONFIG), "{p:?}");
}
