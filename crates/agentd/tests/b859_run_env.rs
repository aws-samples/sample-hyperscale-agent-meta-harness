//! A direct run starts its engine with git's settings for the run's own folders.
//!
//! `tests/b859_scratch_git.rs` proves with real git what the settings do. This test proves that the driver itself puts them in the
//! environment of the engine it starts, for the repository and the temp folder of that run: the whole run in process, against the fake AWS
//! and the fake engine, which writes down the directory it was started in and its environment. No AWS call is made.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts};
use std::collections::BTreeMap;

#[tokio::test(flavor = "multi_thread")]
async fn b859_a_direct_run_starts_the_engine_with_gits_settings_for_its_own_folders() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let probe = d.engine_probe();
    let started_in = probe.lines().next().unwrap_or_default().to_string();
    let env: BTreeMap<&str, &str> = probe
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .collect();
    let repo = d.worktree().display().to_string();
    let tmp = d.agent_data().join("tmp").display().to_string();
    assert_eq!(
        started_in, repo,
        "the engine starts in the run's repository"
    );
    assert_eq!(env.get("TMPDIR").copied(), Some(tmp.as_str()));
    assert_eq!(
        env.get("GIT_CONFIG_COUNT").copied(),
        Some("3"),
        "the engine's environment should carry three safe.directory settings for the run's folders"
    );
    let wanted = [repo.clone(), format!("{repo}/*"), format!("{tmp}/*")];
    for (i, value) in wanted.iter().enumerate() {
        assert_eq!(
            env.get(format!("GIT_CONFIG_KEY_{i}").as_str()).copied(),
            Some("safe.directory")
        );
        assert_eq!(
            env.get(format!("GIT_CONFIG_VALUE_{i}").as_str()).copied(),
            Some(value.as_str())
        );
    }
}
