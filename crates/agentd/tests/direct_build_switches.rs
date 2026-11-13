//! The switches for four defaults of the session: cargo built without debug information, cargo built without
//! incremental state, `target/` on the repository's exclude list, and the sentence about work outside the checkout in the goal prompts.
//! Each is on by default and the start request can turn it off (`build` and `goal.outsideCheckoutNote`); with no member the run is the run it
//! always was.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::cargo::{build_env, build_env_with};
use agentd::direct::goal::settings::{DEFAULT_GOAL_TEXT, GoalSettings, OUTSIDE_CHECKOUT_NOTE};
use agentd::direct::goal::{KICK_PROMPT, kick_prompt};
use agentd::workspace::{
    commit_turn, exclude_generated, exclude_generated_with, head, prepare_local,
};
use common::{BASE_CALC, git, python_calc, scratch};
use direct_support::{Direct, Opts};
use harness_protocol::direct::{BuildOptions, GoalOptions};
use serde_json::json;
use std::collections::BTreeMap;

const DEBUG_KEYS: [&str; 2] = ["CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"];

fn off() -> BuildOptions {
    BuildOptions {
        cargo_no_debug_info: Some(false),
        cargo_no_incremental: Some(false),
        exclude_target_dir: Some(false),
    }
}

// ---- cargo's environment ------------------------------------------------------------------------------------------------

#[test]
fn without_a_build_member_the_environment_is_the_one_the_session_always_had() {
    assert_eq!(build_env_with(2, &BuildOptions::default()), build_env(2));
    let env = build_env(2);
    for k in DEBUG_KEYS {
        assert_eq!(env.get(k).map(String::as_str), Some("0"), "{k}");
    }
    assert_eq!(env.get("CARGO_INCREMENTAL").map(String::as_str), Some("0"));
    // Said out loud, `true` is the default.
    let on = BuildOptions {
        cargo_no_debug_info: Some(true),
        cargo_no_incremental: Some(true),
        exclude_target_dir: Some(true),
    };
    assert_eq!(build_env_with(2, &on), env);
}

#[test]
fn turning_debug_information_off_leaves_the_two_profile_variables_to_cargo() {
    let env = build_env_with(
        3,
        &BuildOptions {
            cargo_no_debug_info: Some(false),
            ..BuildOptions::default()
        },
    );
    for k in DEBUG_KEYS {
        assert!(!env.contains_key(k), "{k} is not set: {env:?}");
    }
    assert_eq!(env.get("CARGO_INCREMENTAL").map(String::as_str), Some("0"));
    assert_eq!(env.get("CARGO_BUILD_JOBS").map(String::as_str), Some("3"));
}

#[test]
fn turning_the_incremental_default_off_leaves_that_variable_to_cargo() {
    let env = build_env_with(
        2,
        &BuildOptions {
            cargo_no_incremental: Some(false),
            ..BuildOptions::default()
        },
    );
    assert!(!env.contains_key("CARGO_INCREMENTAL"), "{env:?}");
    for k in DEBUG_KEYS {
        assert_eq!(env.get(k).map(String::as_str), Some("0"), "{k}");
    }
}

#[test]
fn with_every_switch_off_only_the_job_count_the_backtrace_and_the_git_client_remain() {
    let env = build_env_with(2, &off());
    let keys: Vec<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "CARGO_BUILD_JOBS",
            "CARGO_NET_GIT_FETCH_WITH_CLI",
            "RUST_BACKTRACE"
        ]
    );
}

// ---- the exclude list --------------------------------------------------------------------------------------------------------

fn exclude_lines(repo: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(repo.join(".git/info/exclude"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn the_default_exclude_list_has_target_and_the_off_switch_leaves_it_out() {
    let d = scratch("b646-exclude");
    let src = python_calc(&d);
    let on = prepare_local(&d.join("ws"), "run1", "a1", &src)
        .await
        .unwrap();
    exclude_generated(&on).unwrap();
    let on_lines = exclude_lines(&on.repo);
    for entry in ["__pycache__/", "*.py[cod]", "target/"] {
        assert!(on_lines.iter().any(|l| l == entry), "{entry}: {on_lines:?}");
    }
    let same = prepare_local(&d.join("ws2"), "run2", "a2", &src)
        .await
        .unwrap();
    exclude_generated_with(&same, true).unwrap();
    assert_eq!(exclude_lines(&same.repo), on_lines);

    let wt = prepare_local(&d.join("ws3"), "run3", "a3", &src)
        .await
        .unwrap();
    exclude_generated_with(&wt, false).unwrap();
    let lines = exclude_lines(&wt.repo);
    assert!(lines.iter().any(|l| l == "__pycache__/"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "*.py[cod]"), "{lines:?}");
    assert!(!lines.iter().any(|l| l == "target/"), "{lines:?}");
}

#[tokio::test]
async fn with_target_off_the_list_a_build_folder_reaches_the_turn_commit() {
    // The same setting as `cargo_build_output_never_reaches_the_turn_commit`, with the switch off: the folder is not ignored, so the
    // commit takes it up (a repository that ignores it itself is unaffected; this one has no .gitignore).
    let d = scratch("b646-target-off");
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run4", "a4", &src)
        .await
        .unwrap();
    exclude_generated_with(&wt, false).unwrap();
    let base = head(&wt).await.unwrap();
    let out = wt.repo.join("target/debug/deps/libx.rlib");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, "rlib").unwrap();
    std::fs::write(
        wt.repo.join("src/calc.py"),
        format!("{BASE_CALC}\n\ndef sub(a, b):\n    return a - b\n"),
    )
    .unwrap();
    let new_head = commit_turn(&wt, 1).await.unwrap();
    let files = git(&wt.repo, &["diff", "--name-only", &base, &new_head]);
    assert!(files.contains("target/debug/deps/libx.rlib"), "{files}");
    assert!(files.contains("src/calc.py"), "{files}");
}

// ---- the goal prompts ----------------------------------------------------------------------------------------------------------

#[test]
fn the_note_is_in_the_goal_text_and_the_continue_prompt_by_default() {
    let s = GoalSettings::from_lookup(&|_| None);
    assert!(s.outside_note);
    assert_eq!(s.goal_text, DEFAULT_GOAL_TEXT);
    assert!(DEFAULT_GOAL_TEXT.ends_with(OUTSIDE_CHECKOUT_NOTE));
    assert!(KICK_PROMPT.ends_with(OUTSIDE_CHECKOUT_NOTE));
    assert_eq!(s.kick_prompt(), KICK_PROMPT);
    assert_eq!(kick_prompt(true), KICK_PROMPT);
    // `true` in the request is the default said out loud.
    let said = s.with_request(&GoalOptions {
        outside_checkout_note: Some(true),
        ..GoalOptions::switch(true)
    });
    assert_eq!(said, s);
}

#[test]
fn the_off_switch_takes_the_sentence_out_of_both_prompts_and_keeps_the_rest() {
    let s = GoalSettings::from_lookup(&|_| None).with_request(&GoalOptions {
        outside_checkout_note: Some(false),
        ..GoalOptions::switch(true)
    });
    assert!(!s.outside_note);
    assert!(!s.goal_text.to_lowercase().contains("outside this checkout"));
    assert!(!s.goal_text.contains("NOT DONE"), "{}", s.goal_text);
    assert!(s.goal_text.contains("BLOCKED:"), "the blocked line stays");
    assert!(
        s.goal_text.contains(".ham/lifecycle/GOAL.md"),
        "the goal card stays"
    );
    assert!(
        s.goal_text.ends_with("says what is missing."),
        "{}",
        s.goal_text
    );
    let kick = s.kick_prompt();
    assert!(
        !kick.to_lowercase().contains("outside this checkout"),
        "{kick}"
    );
    assert!(kick.starts_with("Continue working toward the goal."));
    assert!(kick.ends_with("says what is missing."), "{kick}");
    assert_eq!(kick, kick_prompt(false));
    assert_ne!(kick, KICK_PROMPT);
    // An operator's own goal text keeps its words, apart from the sentence.
    let custom = GoalSettings::from_lookup(&|k| {
        (k == "AGENTD_GOAL_TEXT").then(|| format!("Make the tests pass. {OUTSIDE_CHECKOUT_NOTE}"))
    })
    .with_request(&GoalOptions {
        outside_checkout_note: Some(false),
        ..GoalOptions::switch(true)
    });
    assert_eq!(custom.goal_text, "Make the tests pass.");
}

// ---- the whole run, through the request --------------------------------------------------------------------------------------

/// The environment the fake engine printed (`--- env` section of its probe).
fn probe_env(d: &Direct) -> BTreeMap<String, String> {
    d.engine_probe()
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_a_build_member_gets_the_three_defaults() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let env = probe_env(&d);
    for k in DEBUG_KEYS {
        assert_eq!(env.get(k).map(String::as_str), Some("0"), "{k}");
    }
    assert_eq!(env.get("CARGO_INCREMENTAL").map(String::as_str), Some("0"));
    let exclude = exclude_lines(&d.worktree());
    assert!(exclude.iter().any(|l| l == "target/"), "{exclude:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_turns_each_default_off_for_one_run() {
    let d = Direct::start(Opts {
        build: Some(json!({"cargoNoDebugInfo": false})),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let env = probe_env(&d);
    for k in DEBUG_KEYS {
        assert!(!env.contains_key(k), "{k}: {env:?}");
    }
    assert_eq!(env.get("CARGO_INCREMENTAL").map(String::as_str), Some("0"));
    assert!(exclude_lines(&d.worktree()).iter().any(|l| l == "target/"));

    let d = Direct::start(Opts {
        build: Some(json!({"cargoNoIncremental": false})),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let env = probe_env(&d);
    assert!(!env.contains_key("CARGO_INCREMENTAL"), "{env:?}");
    assert_eq!(
        env.get("CARGO_PROFILE_DEV_DEBUG").map(String::as_str),
        Some("0")
    );

    let d = Direct::start(Opts {
        build: Some(json!({"excludeTargetDir": false})),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let exclude = exclude_lines(&d.worktree());
    assert!(!exclude.iter().any(|l| l == "target/"), "{exclude:?}");
    assert!(exclude.iter().any(|l| l == "__pycache__/"), "{exclude:?}");
    assert_eq!(
        probe_env(&d).get("CARGO_INCREMENTAL").map(String::as_str),
        Some("0")
    );
}
