//! A clean-checkout test command that was stopped at its limit while it was still building says so. A 15-member Rust workspace ran the
//! harness-chosen `cargo test`; the report stopped it at 300 s with only `Compiling` lines in its output, and the card said "`cargo test` timed out
//! after 300 s; the commands that finished are listed in the report.", which reads like a test run that took too long. No test had started: the
//! report says nothing about the tests, and the card says that, counts it as neither passed nor failed, and names the limit.

#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    TestCommandSource, TestReport, TestReportStatus, TestStep, TestStepRole,
};

fn timed_out(command: &str, tail: &str) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: command.into(),
        exit_code: None,
        timed_out: true,
        duration_ms: 300_000,
        limit_s: Some(300),
        counts: None,
        counts_from: None,
        output_tail: tail.into(),
        output_truncated: false,
        logged: None,
    }
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::TimedOut,
        reason: None,
        started_at: None,
        duration_ms: 300_000,
        steps,
        summary: None,
    }
}

const CARGO_BUILD: &str = "   Compiling proc-macro2 v1.0.89\n   Compiling unicode-ident v1.0.13\n    Updating crates.io index\n  Downloaded serde v1.0.214\nwarning: unused variable: `x`\n --> crates/a/src/lib.rs:3:9\n  |\n3 |     let x = 1;\n  |         ^ help: if this is intentional, prefix it with an underscore: `_x`\n   Compiling harness-core v0.1.0 (/w/crates/harness-core)\n";

#[test]
fn b664_a_command_stopped_while_it_was_still_building_says_no_test_had_started() {
    let s = report(vec![timed_out("cargo test", CARGO_BUILD)]).summary();
    assert!(s.stopped_while_building(), "{s:?}");
    assert_eq!(s.timed_out.clone().unwrap().after_s, 300);
    assert_eq!(
        s.sentence(),
        "`cargo test` was stopped at its limit of 300 s while it was still building: its output shows build work and no test that began, so this report says nothing about the tests."
    );
    // Neither passed nor failed: no claim, no counts, not all exit zero.
    assert_eq!(s.claim(), None);
    assert_eq!(s.counts, None);
    assert!(!s.all_exit_zero);
    // The note of the limit still names the setting that lengthens it.
    assert!(
        s.limit_note()
            .unwrap()
            .contains("run.report_command_minutes"),
        "{:?}",
        s.limit_note()
    );
}

#[test]
fn b664_go_and_maven_downloads_count_as_building() {
    for (command, tail) in [
        (
            "go test -v ./...",
            "go: downloading github.com/stretchr/testify v1.9.0\ngo: downloading gopkg.in/yaml.v3 v3.0.1\n",
        ),
        (
            "mvn -B test",
            "[INFO] Downloading from central: https://repo.maven.apache.org/maven2/a.pom\n[INFO] Downloaded from central: https://repo.maven.apache.org/maven2/a.pom (2 kB)\n[INFO] Compiling 12 source files\n",
        ),
    ] {
        // The lines are build work. That no test began is said for cargo alone (the review of the 5.15 head): the go tool holds a package's
        // lines back, Maven runs tests through plugins of any name, and `make` may start any program (`fix515_b664_no_test_began.rs`).
        for command in [command, "make test"] {
            let s = report(vec![timed_out(command, tail)]).summary();
            assert!(!s.stopped_while_building(), "{command}: {tail}");
            assert!(s.stopped_in_build_work(), "{command}: {tail}");
        }
    }
}

#[test]
fn b664_a_command_stopped_after_its_tests_began_reads_as_before() {
    for tail in [
        "   Compiling a v0.1.0\n    Finished `test` profile [unoptimized] target(s) in 41.2s\n     Running unittests src/lib.rs (target/debug/deps/a-1)\n\nrunning 12 tests\ntest x ... ok\n",
        "============================= test session starts ==============================\ncollected 40 items\n",
        "=== RUN   TestSlow\n",
    ] {
        let s = report(vec![timed_out("cargo test", tail)]).summary();
        assert!(!s.stopped_while_building(), "{tail}");
        assert_eq!(
            s.sentence(),
            "`cargo test` timed out after 300 s; the commands that finished are listed in the report."
        );
    }
}

#[test]
fn b664_an_empty_or_unknown_output_is_not_called_building() {
    for tail in ["", "waiting for the database\nstill waiting\n"] {
        let s = report(vec![timed_out("bash scripts/test.sh", tail)]).summary();
        assert!(!s.stopped_while_building(), "{tail:?}");
    }
}

#[test]
fn b664_the_summary_carries_the_fact_in_its_reason_so_every_reader_of_the_summary_alone_says_the_same()
 {
    // EVIDENCE.md, the pull request comment and `ham pull` read the summary without the steps: the fact rides in `reason`, a member every reader has.
    let s = report(vec![timed_out("cargo test", CARGO_BUILD)]).summary();
    let j = serde_json::to_value(&s).unwrap();
    assert!(
        j["reason"]
            .as_str()
            .unwrap()
            .starts_with(harness_protocol::direct::STILL_BUILDING_REASON),
        "{j}"
    );
    let back: harness_protocol::direct::TestReportSummary = serde_json::from_value(j).unwrap();
    assert_eq!(back.sentence(), s.sentence());
    assert!(back.stopped_while_building());
    assert!(!back.stopped_in_build_work());
}
