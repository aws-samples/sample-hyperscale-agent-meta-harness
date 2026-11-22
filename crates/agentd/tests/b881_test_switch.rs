//! The switch of the live test row and its limits (the lead's ruling 2).
//!
//! A real refusal of the session's storage cannot be made on demand, so one switch makes the worker answer one test write with
//! errno 28. Its limits: only a worker built for an account entry with the role `test` reads it; in a build for any other entry the
//! same variable does nothing; no request member, no user setting and no file of a work tree reaches it; it answers one test write and
//! never again in that session; the log and the notice event say that the switch made the answer. The last two are held by
//! `b881_storage_watch.rs` and `b881_whole_run.rs`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::config::{
    DirectConfig, STORAGE_TEST_REFUSAL_VAR, built_for_role, storage_test_refusal,
};
use std::path::{Path, PathBuf};

fn var(value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |k: &str| {
        (k == STORAGE_TEST_REFUSAL_VAR)
            .then(|| value.map(str::to_string))
            .flatten()
    }
}

#[test]
fn b881_only_a_worker_built_for_a_test_entry_reads_the_switch() {
    assert_eq!(STORAGE_TEST_REFUSAL_VAR, "AGENTD_TEST_STORAGE_REFUSAL");
    assert!(storage_test_refusal(&var(Some("1")), "test"));
    assert!(storage_test_refusal(&var(Some(" 1 ")), "test"));
    // Only the value 1 switches it on.
    for off in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("yes"),
        Some("on"),
        Some("2"),
    ] {
        assert!(!storage_test_refusal(&var(off), "test"), "{off:?}");
    }
}

#[test]
fn b881_in_a_build_for_any_other_entry_the_same_variable_does_nothing() {
    for role in ["active", "history", "", "Test", "official"] {
        assert!(!storage_test_refusal(&var(Some("1")), role), "{role:?}");
    }
    // The binary of this test run carries its own selection: the config follows it and nothing else.
    let role = built_for_role();
    assert!(["active", "history", "test"].contains(&role), "{role:?}");
    let on = DirectConfig::from_vars(&var(Some("1")));
    assert_eq!(on.storage_test_refusal, role == "test");
    // Without the variable it is off in every build.
    assert!(!DirectConfig::from_vars(&var(None)).storage_test_refusal);
    assert!(!DirectConfig::default().storage_test_refusal);
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "target") {
                rust_files(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn b881_no_request_member_user_setting_or_file_of_the_work_tree_reaches_the_switch() {
    let root = common::repo_root();
    let mut files = Vec::new();
    for krate in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(
        files.len() > 200,
        "the product's sources were not found: {}",
        files.len()
    );
    let rel = |p: &Path| p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
    // The variable is named in one source file: the worker's own config, which reads the worker's process environment.
    let named: Vec<String> = files
        .iter()
        .filter(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .contains("TEST_STORAGE_REFUSAL")
        })
        .map(|p| rel(p))
        .collect();
    assert_eq!(named, vec!["crates/agentd/src/config.rs".to_string()]);
    // The config's member is read in one place: where the watch of a session is built.
    let mut read: Vec<String> = files
        .iter()
        .filter(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .contains("storage_test_refusal")
        })
        .map(|p| rel(p))
        .collect();
    read.sort();
    assert_eq!(
        read,
        vec![
            "crates/agentd/src/config.rs".to_string(),
            "crates/agentd/src/direct/mod.rs".to_string()
        ]
    );
    // No wire type, no client setting and no schema knows a word for it: a request or a setting cannot carry it.
    for dir in [
        "crates/harness-protocol/src",
        "crates/ham-core/src",
        "crates/ham/src",
        "crates/control-api/src",
        "crates/run-session/src",
        "crates/worker-api/src",
    ] {
        let mut inner = Vec::new();
        rust_files(&root.join(dir), &mut inner);
        for p in inner {
            let text = std::fs::read_to_string(&p).unwrap().to_lowercase();
            for word in [
                "storagetestrefusal",
                "teststoragerefusal",
                "test_storage_refusal",
                "storage_test_refusal",
            ] {
                assert!(!text.contains(word), "{word} in {}", rel(&p));
            }
        }
    }
    for e in std::fs::read_dir(root.join("schemas")).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_default()
                .to_lowercase();
            assert!(
                !text.contains("storagerefusal")
                    && !text.contains("storage_refusal")
                    && !text.contains("testrefusal"),
                "{}",
                rel(&p)
            );
        }
    }
    // The engine's environment is built from a fixed list and the run's own values (`engine::launch`); the worker's own environment is
    // not handed on, so the variable is not something an engine or a command of a run can read back or set for the worker.
    let launch = std::fs::read_to_string(root.join("crates/agentd/src/engine/launch.rs")).unwrap();
    assert!(
        !launch.contains("std::env::vars()"),
        "the engine's environment copies the worker's"
    );
}
