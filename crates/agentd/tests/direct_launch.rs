//! What a direct run adds to the engine's launch. The Cline profile is
//! shared with the old line, which answers the engine's permission requests from the role policy: its argv must stay the one it
//! had, and `--auto-approve true` belongs to the direct launch only.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::direct::direct_launch_argv;
use agentd::engine::user_settings::Effort;
use common::repo_root;
use harness_core::engine_profile::load_profile;

fn words(a: &[&str]) -> Vec<String> {
    a.iter().map(ToString::to_string).collect()
}

fn profile_argv(name: &str) -> Vec<String> {
    load_profile(&repo_root().join(format!("engine/profiles/{name}.toml")))
        .unwrap()
        .transports
        .acp
        .argv
}

#[test]
fn the_cline_profile_keeps_the_old_lines_argv() {
    assert_eq!(
        profile_argv("cline"),
        words(&[
            "/opt/harness/bin/cline",
            "--acp",
            "--config",
            "{configDir}",
            "--data-dir",
            "{dataDir}"
        ])
    );
}

#[test]
fn the_direct_launch_of_cline_is_the_contracts_argv() {
    assert_eq!(
        direct_launch_argv("cline", profile_argv("cline"), Effort::Default),
        words(&[
            "/opt/harness/bin/cline",
            "--acp",
            "--auto-approve",
            "true",
            "--config",
            "{configDir}",
            "--data-dir",
            "{dataDir}"
        ])
    );
}

#[test]
fn the_flag_is_named_once_and_only_for_cline() {
    let once = direct_launch_argv("cline", profile_argv("cline"), Effort::Default);
    assert_eq!(
        direct_launch_argv("cline", once.clone(), Effort::Default),
        once,
        "a second pass adds nothing"
    );
    for engine in ["fake-acp", "opencode"] {
        let argv = profile_argv(engine);
        assert_eq!(
            direct_launch_argv(engine, argv.clone(), Effort::Default),
            argv,
            "{engine}"
        );
    }
}

#[test]
fn without_acp_the_flag_follows_the_program_and_an_empty_argv_is_left_alone() {
    assert_eq!(
        direct_launch_argv("cline", words(&["wrapper", "--x", "y"]), Effort::Default),
        words(&["wrapper", "--auto-approve", "true", "--x", "y"])
    );
    // Nothing to launch: no flag is made up for it to carry.
    assert!(direct_launch_argv("cline", Vec::new(), Effort::Default).is_empty());
}
