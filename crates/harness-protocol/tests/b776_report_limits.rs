//! The clean-checkout test report keeps what a person needs to find the cause. The output of a step is no longer cut to its last
//! 4 KiB, a report holds more than 12 steps, and the report may be as large as the object bound the readers can hold (the run-session
//! Lambda has 256 MB). The time one command may run reaches the session bound.
#![allow(clippy::unwrap_used, clippy::assertions_on_constants)] // the tests pin the values of constants on purpose

use harness_protocol::direct::{
    DEFAULT_REPORT_TAIL_BYTES, MAX_DIRECT_TIMEOUT_S, MAX_REPORT_BYTES, MAX_REPORT_COMMAND_CHARS,
    MAX_REPORT_REASON_CHARS, MAX_REPORT_STEP_S, MAX_REPORT_STEPS, MAX_REPORT_TAIL_BYTES,
    TestCommandSource, TestReport, TestReportStatus, TestStep, TestStepRole, report_step_s_ok,
};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn step(tail: String) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: "cargo test".to_string(),
        exit_code: Some(101),
        timed_out: false,
        duration_ms: 1200,
        limit_s: None,
        counts: None,
        counts_from: None,
        output_tail: tail,
        output_truncated: false,
        logged: None,
    }
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: RUN.to_string(),
        head_sha: HEAD.to_string(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 4_200,
        steps,
        summary: None,
    }
}

/// Lines of plain test output, `n` bytes long at least.
fn output(n: usize) -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < n {
        s.push_str(&format!(
            "test case_{i} ... FAILED: assertion left == right\n"
        ));
        i += 1;
    }
    s
}

#[test]
fn b776_a_step_that_printed_200_kib_keeps_all_of_it() {
    let out = output(200 * 1024);
    let r = report(vec![step(out.clone())]);
    r.validate(RUN, HEAD).unwrap();
    let b = r.bounded();
    assert_eq!(b.steps[0].output_tail, out, "the whole output is kept");
    assert!(!b.steps[0].output_truncated);
    b.validate(RUN, HEAD).unwrap();
}

#[test]
fn b776_the_default_tail_is_generous_and_the_ceiling_is_above_it() {
    assert!(
        DEFAULT_REPORT_TAIL_BYTES >= 256 * 1024,
        "{DEFAULT_REPORT_TAIL_BYTES}"
    );
    assert!(MAX_REPORT_TAIL_BYTES >= DEFAULT_REPORT_TAIL_BYTES);
    // The ceiling is what validate accepts; one byte more is refused.
    let mut r = report(vec![step("x".repeat(MAX_REPORT_TAIL_BYTES + 1))]);
    assert!(r.validate(RUN, HEAD).is_err());
    r.steps[0].output_tail = "x".repeat(MAX_REPORT_TAIL_BYTES);
    r.validate(RUN, HEAD).unwrap();
}

#[test]
fn b776_a_report_holds_32_steps_and_refuses_a_33rd() {
    assert!(MAX_REPORT_STEPS >= 32, "{MAX_REPORT_STEPS}");
    let steps: Vec<TestStep> = (0..MAX_REPORT_STEPS)
        .map(|_| step(output(8 * 1024)))
        .collect();
    report(steps.clone()).validate(RUN, HEAD).unwrap();
    let mut over = steps;
    over.push(step("one more".into()));
    assert!(report(over.clone()).validate(RUN, HEAD).is_err());
    assert_eq!(report(over).bounded().steps.len(), MAX_REPORT_STEPS);
}

#[test]
fn b776_the_report_may_be_larger_than_64_kib() {
    assert!(MAX_REPORT_BYTES >= 4 * 1024 * 1024, "{MAX_REPORT_BYTES}");
    let r = report((0..12).map(|_| step(output(100 * 1024))).collect());
    let bytes = serde_json::to_vec(&r).unwrap().len();
    assert!(bytes > 1024 * 1024, "{bytes}");
    r.validate(RUN, HEAD).unwrap();
    let b = r.clone().bounded();
    assert_eq!(b, r, "nothing is cut when the report fits the bound");
}

#[test]
fn b776_output_that_json_doubles_still_fits_the_object_bound_by_halving_the_longest_tail() {
    // Every byte of this output is escaped to two bytes, so six steps at the tail ceiling are well over the bound once serialized.
    let body = "\n\"\\".repeat(MAX_REPORT_TAIL_BYTES / 3);
    let r = report((0..6).map(|_| step(body.clone())).collect());
    assert!(serde_json::to_vec(&r).unwrap().len() > MAX_REPORT_BYTES);
    let b = r.bounded();
    assert!(serde_json::to_vec(&b).unwrap().len() <= MAX_REPORT_BYTES);
    assert_eq!(b.steps.len(), 6);
    assert!(b.steps.iter().all(|s| s.output_truncated));
    assert!(
        b.steps.iter().all(|s| !s.output_tail.is_empty()),
        "every step keeps the end of its output"
    );
    b.validate(RUN, HEAD).unwrap();
}

#[test]
fn b776_a_long_command_and_a_long_reason_are_kept() {
    assert!(MAX_REPORT_COMMAND_CHARS >= 2_000);
    assert!(MAX_REPORT_REASON_CHARS >= 1_000);
    let mut s = step("ok".into());
    s.command = format!("cargo test {}", "-p some-crate ".repeat(120));
    assert!(s.command.chars().count() > 1_000);
    let mut r = report(vec![s.clone()]);
    r.validate(RUN, HEAD).unwrap();
    assert_eq!(r.clone().bounded().steps[0].command, s.command);
    r.status = TestReportStatus::NotRun;
    r.steps.clear();
    r.reason = Some("r".repeat(900));
    r.validate(RUN, HEAD).unwrap();
    assert_eq!(r.clone().bounded().reason, r.reason);
}

#[test]
fn b776_one_command_may_run_as_long_as_the_session_does() {
    assert_eq!(MAX_REPORT_STEP_S, MAX_DIRECT_TIMEOUT_S);
    assert!(report_step_s_ok(3_601));
    assert!(report_step_s_ok(MAX_REPORT_STEP_S));
    assert!(!report_step_s_ok(MAX_REPORT_STEP_S + 1));
    assert!(!report_step_s_ok(0));
    let mut s = step("ok".into());
    s.limit_s = Some(MAX_REPORT_STEP_S);
    report(vec![s.clone()]).validate(RUN, HEAD).unwrap();
    s.limit_s = Some(MAX_REPORT_STEP_S + 1);
    assert!(report(vec![s]).validate(RUN, HEAD).is_err());
}
