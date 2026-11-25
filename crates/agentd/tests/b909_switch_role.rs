//! Who reads the test switch of the storage notice.
//!
//! The worker honours `AGENTD_TEST_STORAGE_REFUSAL` of its own environment, with the exact value `1`, when it was built for an
//! account entry with the role `active` or `test`. A build for the role `history` or for an unknown role ignores it. The worker
//! never learns the stage's kind: the deploy decides whether a runtime carries the variable. What a session can set (the engine's
//! environment, a file of the work tree) still does not reach the switch, and the worker says in one line of its log, at its start,
//! that the switch is on.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::{
    DirectConfig, STORAGE_TEST_REFUSAL_VAR, built_for_role, storage_test_refusal,
    test_switch_start_line,
};
use direct_support::{Direct, Opts};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use std::time::{Duration, Instant};

/// The line of the worker's log at its start, word for word.
const START_LINE: &str = "agentd: the stage's test switch is on (AGENTD_TEST_STORAGE_REFUSAL=1): one test write of each session and the first write of the git settings at a session's end, once, are answered with a refusal, without touching the storage";

fn var(value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |k: &str| {
        (k == STORAGE_TEST_REFUSAL_VAR)
            .then(|| value.map(str::to_string))
            .flatten()
    }
}

/// TRIGGER: the build of this test run reads the switch, so its role does not shield the rows that say the switch stays off.
fn this_build_reads_the_switch() {
    let role = built_for_role();
    println!("b909: this test build was made for an account entry with the role {role:?}");
    assert!(
        role == "active" || role == "test",
        "TRIGGER PRESENT is not shown: this build's role is {role:?}, and a build for that role ignores the variable"
    );
    assert!(
        DirectConfig::from_vars(&var(Some("1"))).storage_test_refusal,
        "TRIGGER PRESENT is not shown: the config of this build ignores the variable in the worker's own environment"
    );
}

#[test]
fn b909_w1_the_roles_active_and_test_read_the_switch_and_no_other_role_does() {
    for role in ["active", "test"] {
        assert!(storage_test_refusal(&var(Some("1")), role), "{role:?}");
    }
    for role in ["history", "", "Test", "Active", "ACTIVE", "official"] {
        assert!(!storage_test_refusal(&var(Some("1")), role), "{role:?}");
    }
}

#[test]
fn b909_w2_only_the_value_1_switches_it_on_for_both_roles() {
    for role in ["active", "test"] {
        assert!(storage_test_refusal(&var(Some("1")), role), "{role:?}");
        assert!(storage_test_refusal(&var(Some(" 1 ")), role), "{role:?}");
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some("on"),
            Some("2"),
            Some("11"),
        ] {
            assert!(!storage_test_refusal(&var(off), role), "{role:?} {off:?}");
        }
    }
}

#[test]
fn b909_w3_the_config_of_this_build_follows_its_role() {
    let role = built_for_role();
    println!("b909: this test build was made for an account entry with the role {role:?}");
    assert!(["active", "history", "test"].contains(&role), "{role:?}");
    let on = DirectConfig::from_vars(&var(Some("1")));
    assert_eq!(
        on.storage_test_refusal,
        role == "active" || role == "test",
        "{role:?}"
    );
    // Any other value is off in every build, and so is no variable.
    assert!(!DirectConfig::from_vars(&var(Some("on"))).storage_test_refusal);
    assert!(!DirectConfig::from_vars(&var(None)).storage_test_refusal);
    assert!(!DirectConfig::default().storage_test_refusal);
}

fn storage_notices(d: &Direct) -> Vec<serde_json::Value> {
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == "storage_refused")
        .collect()
}

/// What a session can set: the variable in the engine's own environment, and files of the work tree that carry its name.
fn what_a_session_can_set() -> Opts {
    Opts {
        scenario: "no_space_result",
        engine_env: vec![("AGENTD_TEST_STORAGE_REFUSAL", "1".into())],
        repo_files: vec![
            (".env", "AGENTD_TEST_STORAGE_REFUSAL=1\n"),
            ("AGENTD_TEST_STORAGE_REFUSAL", "1\n"),
            (".ham/test-storage-refusal", "1\n"),
        ],
        ..Opts::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b909_w4_an_engines_environment_and_files_of_the_work_tree_cannot_turn_the_switch_on() {
    this_build_reads_the_switch();
    let d = Direct::start(what_a_session_can_set()).await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(storage_notices(&d).is_empty(), "{:?}", storage_notices(&d));
    let lines: Vec<String> = d
        .log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn storage"))
        .collect();
    assert!(
        lines.iter().any(|l| l == "direct turn storage: a tool result named no space left on device; a test write to the session's storage worked"),
        "{lines:?}"
    );
    assert!(
        !d.log.lines().iter().any(|l| l.contains("test switch")),
        "{:?}",
        d.log.lines()
    );
    assert_eq!(d.summary().label, "Cline reported done");
}

fn process_log(d: &Direct) -> String {
    std::fs::read_to_string(&d.process.as_ref().unwrap().log_file).unwrap()
}

/// The same row on the real `agentd serve` process, which reads its settings from its own environment: the variable stands in the
/// engine's environment and in the work tree and not in the worker's, so the switch is off and the worker says nothing about it.
#[tokio::test(flavor = "multi_thread")]
async fn b909_w4_the_real_worker_reads_its_own_environment_alone() {
    this_build_reads_the_switch();
    let d = Direct::start_process(Opts {
        timeout_s: 600,
        ..what_a_session_can_set()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(storage_notices(&d).is_empty(), "{:?}", storage_notices(&d));
    let log = process_log(&d);
    assert!(log.contains("agentd: listening on "), "{log}");
    assert!(
        log.contains("a tool result named no space left on device; a test write to the session's storage worked"),
        "{log}"
    );
    assert!(!log.contains("test switch"), "{log}");
}

#[test]
fn b909_w5_the_start_line_is_given_only_with_the_switch_on() {
    let on = DirectConfig {
        storage_test_refusal: true,
        ..DirectConfig::default()
    };
    assert_eq!(test_switch_start_line(&on).as_deref(), Some(START_LINE));
    let line = test_switch_start_line(&on).unwrap();
    assert!(line.contains(STORAGE_TEST_REFUSAL_VAR), "{line}");
    assert!(line.contains("test switch"), "{line}");
    assert_eq!(test_switch_start_line(&DirectConfig::default()), None);
}

/// The real `agentd serve` process writes the line once at its start when its own environment carries the variable with `1`, and
/// writes no such line without it or with another value.
#[tokio::test(flavor = "multi_thread")]
async fn b909_w5_the_real_worker_writes_the_start_line_once_when_the_switch_is_on() {
    this_build_reads_the_switch();
    let mut d = Direct::start_process(Opts {
        scenario: "writes_files",
        agentd_env: vec![(STORAGE_TEST_REFUSAL_VAR, "1".to_string())],
        // The program ends by itself after the signal and the test waits for that, so the lines of its start count in a coverage run.
        writes_profile: true,
        ..Opts::default()
    })
    .await;
    let on = process_log(&d);
    let said: Vec<&str> = on.lines().filter(|l| l.contains("test switch")).collect();
    assert_eq!(said, vec![START_LINE], "{on}");
    // The line follows the line that names the port: it is a line of the start.
    let at = |needle: &str| on.lines().position(|l| l.contains(needle)).unwrap();
    assert!(at("agentd: listening on ") < at("test switch"), "{on}");
    d.process.as_ref().unwrap().signal("TERM");
    let began = Instant::now();
    let ended = loop {
        if d.process.as_mut().unwrap().ended().is_some() {
            break true;
        }
        if began.elapsed() > Duration::from_secs(20) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        ended,
        "the worker ends when it is asked to; log:\n{}",
        process_log(&d)
    );
    let started = |env: Vec<(&'static str, String)>| async move {
        let d = Direct::start_process(Opts {
            scenario: "writes_files",
            agentd_env: env,
            ..Opts::default()
        })
        .await;
        process_log(&d)
    };
    for env in [
        vec![],
        vec![(STORAGE_TEST_REFUSAL_VAR, "on".to_string())],
        vec![(STORAGE_TEST_REFUSAL_VAR, "0".to_string())],
    ] {
        let off = started(env.clone()).await;
        assert!(off.contains("agentd: listening on "), "{env:?}: {off}");
        assert!(!off.contains("test switch"), "{env:?}: {off}");
    }
}
