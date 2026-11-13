//! The clean-checkout report and the services a repository declares. A step that ran, exited with a code other than 0 and said it
//! could not reach a service the report could not supply keeps its exit code, its counts and its output. The report adds a head line that names
//! what the step ran without, the summary reads it, and the card sentence says so in plain words. It never turns a failed step into a passed one
//! and never calls the step "not run": it ran. A service step records what the report started in place of a Compose service, and the claim
//! of the all-pass sentence says that the services were the report's own.
#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    BANNED_CLAIM_WORDS, NEEDS_PREFIX, STAND_IN_PREFIX, TestCommandSource, TestCounts, TestReport,
    TestReportStatus, TestStep, TestStepRole,
};

fn test_step(command: &str, exit: i32, counts: Option<(u32, u32)>, tail: &str) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: command.into(),
        exit_code: Some(exit),
        timed_out: false,
        duration_ms: 10,
        limit_s: Some(300),
        counts: counts.map(|(p, f)| TestCounts {
            passed: p,
            failed: f,
            skipped: 0,
        }),
        counts_from: counts.map(|_| "pytest".to_string()),
        output_tail: tail.into(),
        output_truncated: false,
        logged: None,
    }
}

fn service_step(name: &str, started: bool) -> TestStep {
    let sentence = format!(
        "compose service db (postgres:16 in compose.yaml) has a local {name} server in its place; the Compose container does not run"
    );
    let mut s = TestStep {
        role: TestStepRole::Setup,
        source: TestCommandSource::Detected,
        command: format!("ham-services {name}"),
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 900,
        limit_s: Some(150),
        counts: None,
        counts_from: None,
        output_tail: format!(
            "{STAND_IN_PREFIX}{sentence}\nham-services: postgres ready on 127.0.0.1:5432"
        ),
        output_truncated: false,
        logged: None,
    };
    if !started {
        s = TestStep::not_run(
            s.role,
            s.source,
            &s.command,
            "ham-services is not installed here",
            None,
        );
        s.output_tail =
            format!("NOT RUN: ham-services is not installed here\n{STAND_IN_PREFIX}{sentence}");
    }
    s
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 1000,
        steps,
        summary: None,
    }
}

const REFUSED: &str = "psycopg.OperationalError: connection to server at \"127.0.0.1\", port 5432 failed: Connection refused";

#[test]
fn b600_a_step_that_ran_without_postgres_keeps_its_exit_code_and_counts_and_says_what_it_needed() {
    let failing = test_step(
        "bash scripts/test-integration.sh",
        1,
        Some((0, 6)),
        &format!(
            "{NEEDS_PREFIX}postgres: it could not be started (ham-services is not installed here)\n{REFUSED}"
        ),
    );
    let mut unit = test_step(
        "bash scripts/test-api-unit.sh",
        0,
        Some((42, 0)),
        "42 passed",
    );
    unit.counts_from = Some("vitest".into());
    let r = report(vec![service_step("postgres", false), unit, failing]);
    let s = r.summary();
    assert_eq!(s.needs, ["postgres"]);
    assert_eq!(
        s.exit_code,
        Some(1),
        "the raw exit code is the one the step gave"
    );
    assert_eq!(
        s.failed_command.as_deref(),
        Some("bash scripts/test-integration.sh")
    );
    assert!(!s.all_exit_zero);
    assert_eq!(s.claim(), None);
    assert_eq!(
        s.sentence(),
        "`bash scripts/test-integration.sh` exited 1 in a clean checkout while postgres was not available; the runner reported 42 passed, 6 failed."
    );
    // The step itself is as the command left it, plus the head line.
    let step = &r.steps[2];
    assert!(!step.is_not_run());
    assert_eq!(step.exit_code, Some(1));
    assert_eq!(
        step.counts,
        Some(TestCounts {
            passed: 0,
            failed: 6,
            skipped: 0
        })
    );
    assert_eq!(
        step.needs(),
        [(
            "postgres",
            "it could not be started (ham-services is not installed here)"
        )]
    );
}

#[test]
fn b600_the_details_state_what_each_step_did_and_what_it_needed() {
    let r = report(vec![
        service_step("postgres", false),
        test_step(
            "bash scripts/test-api-unit.sh",
            0,
            Some((42, 0)),
            "42 passed",
        ),
        test_step(
            "bash scripts/test-integration.sh",
            1,
            Some((0, 6)),
            &format!("{NEEDS_PREFIX}postgres: it could not be started\n{REFUSED}"),
        ),
    ]);
    let d = r.summary().details;
    assert_eq!(d.len(), 3, "{d:?}");
    assert!(d[0].starts_with("`ham-services postgres` was not run (ham-services is not installed here). compose service db"), "{d:?}");
    assert_eq!(
        d[1],
        "`bash scripts/test-api-unit.sh`: exit 0, 42 passed, 0 failed."
    );
    assert_eq!(
        d[2],
        "`bash scripts/test-integration.sh`: exit 1, 0 passed, 6 failed; it ran without postgres (it could not be started)."
    );
    assert!(d.iter().all(|l| l.chars().count() <= 300));
}

#[test]
fn b600_started_services_are_named_beside_the_claim_and_the_claim_still_needs_every_step_to_pass() {
    let mut unit = test_step(
        "bash scripts/test-api-unit.sh",
        0,
        Some((42, 0)),
        "42 passed",
    );
    unit.counts_from = Some("vitest".into());
    let r = report(vec![
        service_step("postgres", true),
        unit,
        test_step(
            "bash scripts/test-integration.sh",
            0,
            Some((6, 0)),
            "6 passed",
        ),
    ]);
    let s = r.summary();
    assert_eq!(s.services_started, ["postgres"]);
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 48 of 48")
    );
    assert_eq!(
        s.sentence(),
        "repository tests pass in a clean checkout: 48 of 48. The report started a local postgres server in place of the Compose service the repository declares."
    );
    // A service that could not be started blocks the claim, whatever the tests did.
    let r = report(vec![
        service_step("postgres", false),
        test_step(
            "bash scripts/test-api-unit.sh",
            0,
            Some((42, 0)),
            "42 passed",
        ),
    ]);
    let s = r.summary();
    assert_eq!(s.claim(), None);
    assert!(
        s.sentence().starts_with("NOT RUN: `ham-services postgres`"),
        "{}",
        s.sentence()
    );
}

#[test]
fn b600_a_needs_line_counts_only_where_agentd_writes_it() {
    let line = format!("{NEEDS_PREFIX}postgres: it could not be started\n");
    // A step that exited 0 prints what it likes.
    assert!(
        test_step("bash a.sh", 0, Some((1, 0)), &format!("{line}ok"))
            .needs()
            .is_empty()
    );
    // A line in the middle of the output is the output.
    assert!(
        test_step("bash a.sh", 1, None, &format!("boom\n{line}"))
            .needs()
            .is_empty()
    );
    // A setup step has none.
    let mut setup = test_step("pip install", 1, None, &line);
    setup.role = TestStepRole::Setup;
    assert!(setup.needs().is_empty());
    // A timed-out step has none.
    let mut slow = test_step("bash a.sh", 1, None, &line);
    slow.timed_out = true;
    slow.exit_code = None;
    assert!(slow.needs().is_empty());
    // The line of a service step is read only for a setup step that runs the image's helper.
    let mut other = service_step("postgres", true);
    other.command = "pip install x".into();
    assert!(other.stand_in().is_none());
    assert!(service_step("postgres", true).stand_in().is_some());
}

#[test]
fn b600_a_failing_step_with_no_need_reads_as_before() {
    let r = report(vec![test_step(
        "bash scripts/test-integration.sh",
        1,
        Some((5, 1)),
        "1 failed",
    )]);
    let s = r.summary();
    assert!(s.needs.is_empty());
    assert_eq!(
        s.sentence(),
        "`bash scripts/test-integration.sh` exited 1 in a clean checkout; the runner reported 5 passed, 1 failed."
    );
}

#[test]
fn b600_the_words_of_the_new_sentences_carry_no_banned_claim_word() {
    let r = report(vec![
        service_step("postgres", true),
        test_step(
            "bash scripts/test-integration.sh",
            1,
            Some((0, 6)),
            &format!("{NEEDS_PREFIX}postgres: it could not be started\n"),
        ),
    ]);
    let s = r.summary();
    let mut texts = s.details.clone();
    texts.push(s.sentence());
    let ok = report(vec![
        service_step("postgres", true),
        test_step("bash x.sh", 0, Some((1, 0)), "1 passed"),
    ]);
    texts.push(ok.summary().sentence());
    for t in texts {
        let lower = t.to_lowercase();
        for w in BANNED_CLAIM_WORDS {
            assert!(!lower.contains(w), "{w} in {t}");
        }
    }
}

#[test]
fn b600_an_older_reader_still_reads_a_summary_with_the_new_members_and_a_new_reader_reads_an_older_summary()
 {
    let r = report(vec![
        service_step("postgres", true),
        test_step("bash x.sh", 0, Some((1, 0)), "1 passed"),
    ]);
    let s = r.summary();
    let mut v = serde_json::to_value(&s).unwrap();
    assert!(v["servicesStarted"].is_array() && v["details"].is_array());
    // A summary an older build wrote has none of the three members.
    v.as_object_mut().unwrap().remove("servicesStarted");
    v.as_object_mut().unwrap().remove("details");
    v.as_object_mut().unwrap().remove("needs");
    let back: harness_protocol::direct::TestReportSummary = serde_json::from_value(v).unwrap();
    assert!(back.needs.is_empty() && back.services_started.is_empty() && back.details.is_empty());
    // The report with no service has none of them in its JSON at all.
    let plain = report(vec![test_step("bash x.sh", 0, Some((1, 0)), "1 passed")]).summary();
    let v = serde_json::to_value(&plain).unwrap();
    assert!(v.get("needs").is_none() && v.get("servicesStarted").is_none());
}
