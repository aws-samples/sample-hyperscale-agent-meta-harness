//! The files OpenCode reads its configuration from are the harness's alone: the profile hides them from the repository before
//! launch, and `workspace::hide_paths` moves them aside and back.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{hide_paths, prepare_local, restore_hidden};
use common::{git, python_calc, repo_root, scratch};
use harness_core::engine_profile::{hidden_paths_union, load_profiles};
use std::collections::BTreeSet;

fn opencode_hidden() -> BTreeSet<String> {
    let profiles = load_profiles(&repo_root().join("engine/profiles")).unwrap();
    profiles
        .into_iter()
        .find(|p| p.name == "opencode")
        .unwrap()
        .hidden_paths
        .into_iter()
        .collect()
}

#[test]
fn b790_the_profile_hides_every_name_opencode_reads_its_project_config_from() {
    let hidden = opencode_hidden();
    for must in ["opencode.json", "opencode.jsonc", ".opencode/**"] {
        assert!(hidden.contains(must), "{must}: {hidden:?}");
    }
    // The union over every profile, which agentd hides for every engine, holds them too.
    let all = hidden_paths_union(&load_profiles(&repo_root().join("engine/profiles")).unwrap());
    assert!(all.contains("opencode.jsonc"));
}

#[tokio::test]
async fn b790_the_jsonc_file_and_the_dot_opencode_folder_are_moved_aside_and_put_back() {
    let d = scratch("hide-oc");
    let src = python_calc(&d);
    std::fs::write(
        src.join("opencode.jsonc"),
        "{ // repo config\n  \"share\": \"auto\"\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(src.join(".opencode/agent")).unwrap();
    std::fs::write(src.join(".opencode/agent/x.md"), "agent\n").unwrap();
    git(&src, &["add", "-A"]);
    git(
        &src,
        &[
            "-c",
            "user.email=a@b.invalid",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "repo config",
        ],
    );
    let wt = prepare_local(&d.join("ws"), "run1", "a1", &src)
        .await
        .unwrap();
    assert!(wt.repo.join("opencode.jsonc").is_file());
    let hidden = hide_paths(&wt, &opencode_hidden()).unwrap();
    assert!(!wt.repo.join("opencode.jsonc").exists(), "moved aside");
    assert!(!wt.repo.join(".opencode").exists(), "moved aside");
    assert!(wt.root.join("hidden/opencode.jsonc").is_file());
    restore_hidden(&wt, hidden).unwrap();
    assert!(wt.repo.join("opencode.jsonc").is_file(), "put back");
    assert!(wt.repo.join(".opencode/agent/x.md").is_file(), "put back");
}
