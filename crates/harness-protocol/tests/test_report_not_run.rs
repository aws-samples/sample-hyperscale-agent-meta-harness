//! The constructor and the check of a step that was not run. The behaviour of the summary is pinned in
//! `test_report.rs`; this file pins the API agentd uses.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    NOT_RUN_MARK, NOT_RUN_PREFIX, TestCommandSource, TestStep, TestStepRole,
};

#[test]
fn the_marker_and_the_prefix_are_the_ones_the_documents_name() {
    assert_eq!(NOT_RUN_MARK, "not run");
    assert_eq!(NOT_RUN_PREFIX, "NOT RUN: ");
}

#[test]
fn a_step_not_run_is_built_and_recognised() {
    let s = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::TestCommandFile,
        "docker compose up",
        "the session has no container runtime",
        None,
    );
    assert!(s.is_not_run());
    assert_eq!(s.exit_code, None);
    assert!(!s.timed_out);
    assert_eq!(s.duration_ms, 0);
    assert_eq!(s.counts, None);
    assert_eq!(s.counts_from.as_deref(), Some("not run"));
    assert_eq!(
        s.output_tail,
        "NOT RUN: the session has no container runtime"
    );
}

#[test]
fn an_ordinary_step_is_not_a_step_not_run() {
    let s = TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: "cargo test".into(),
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 5,
        limit_s: None,
        counts: None,
        counts_from: Some("cargo".into()),
        output_tail: "NOT RUN: printed by the test".into(),
        output_truncated: false,
        logged: None,
    };
    assert!(!s.is_not_run());
}
