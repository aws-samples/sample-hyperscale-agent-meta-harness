//! The sentence of a command that was stopped at its limit says "no test that began" only when that is known.
//! The reading of the end of the output knows the lines of cargo, `go test -v`, Maven and Gradle; a command of another runner
//! (mocha's check marks, the dots of `pytest -q`) had a build line and none of those test lines, and the sentence said that no test had begun
//! although tests had run. Now the claim is made only for a command whose runners all print a line the reading knows when their tests begin. For
//! any other command with build lines the sentence says that the output shows build work and that the report cannot tell whether a test began.

#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    STILL_BUILDING_REASON, TestCommandSource, TestReport, TestReportStatus, TestReportSummary,
    TestStep, TestStepRole,
};

/// The first line of the summary's reason for build work in the output of a command whose tests may begin without a line the reading knows. It
/// travels in the summary to every reader, so its words are pinned here.
const BUILD_WORK: &str = "the command was stopped while the end of its output showed build work: whether a test began is not known";

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

fn summary(command: &str, tail: &str) -> TestReportSummary {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::TimedOut,
        reason: Some("a command was stopped at the report time limit".into()),
        started_at: None,
        duration_ms: 300_000,
        steps: vec![timed_out(command, tail)],
        summary: None,
    }
    .summary()
}

fn still_building(command: &str) -> String {
    format!(
        "`{command}` was stopped at its limit of 300 s while it was still building: its output shows build work and no test that began, so this report says nothing about the tests."
    )
}

fn build_work(command: &str) -> String {
    format!(
        "`{command}` was stopped at its limit of 300 s; the end of its output shows build work, and the report cannot tell whether a test began, so it says nothing about the tests."
    )
}

fn plain_timeout(command: &str) -> String {
    format!(
        "`{command}` timed out after 300 s; the commands that finished are listed in the report."
    )
}

const MOCHA: &str = "> app@1.0.0 test\n> npm run build && mocha\n\nBuilding production bundle...\n\n  calc\n    \u{2713} adds two numbers\n    \u{2713} subtracts\n    1) divides by zero\n";
const PIP_THEN_DOTS: &str = "Collecting flask==3.0.3\n  Downloading flask-3.0.3-py3-none-any.whl (101 kB)\nInstalling collected packages: flask\nSuccessfully installed flask-3.0.3\n........................F...............\n";
const CARGO_BUILD: &str = "   Compiling proc-macro2 v1.0.89\n    Updating crates.io index\n  Downloaded serde v1.0.214\nwarning: unused variable: `x`\n   Compiling harness-core v0.1.0 (/w/crates/harness-core)\n";
const GO_DOWNLOADS: &str =
    "go: downloading github.com/stretchr/testify v1.9.0\ngo: downloading gopkg.in/yaml.v3 v3.0.1\n";
const MAVEN_BUILD: &str = "[INFO] Downloading from central: https://repo.maven.apache.org/maven2/a.pom\n[INFO] Compiling 12 source files\n";

#[test]
fn fix515_b664_tests_of_a_runner_the_reading_does_not_know_are_not_said_to_have_not_begun() {
    for (command, tail) in [
        ("npm test", MOCHA),
        (
            "python3 -m pip install -r requirements.txt && python3 -m pytest -q",
            PIP_THEN_DOTS,
        ),
    ] {
        let s = summary(command, tail);
        assert!(!s.stopped_while_building(), "{command}: {s:?}");
        assert_eq!(s.sentence(), build_work(command));
        assert!(!s.sentence().contains("no test"), "{}", s.sentence());
        assert_eq!(s.reason.as_deref(), Some(BUILD_WORK));
        // Neither passed nor failed, and the note of the limit is still there.
        assert_eq!(s.claim(), None);
        assert_eq!(s.counts, None);
        assert!(s.limit_note().unwrap().contains("300 s"));
    }
}

#[test]
fn fix515_b664_a_runner_that_prints_a_line_when_its_tests_begin_is_still_said_to_be_building() {
    for (command, tail) in [
        ("cargo test", CARGO_BUILD),
        ("CI=1 cargo nextest run", CARGO_BUILD),
        // A segment with no program (a `;` at the end) says nothing about the others.
        ("cargo test;", CARGO_BUILD),
        (
            "cd crates && cargo build && cargo test --workspace",
            CARGO_BUILD,
        ),
        ("cargo +nightly test", CARGO_BUILD),
    ] {
        let s = summary(command, tail);
        assert!(s.stopped_while_building(), "{command}: {s:?}");
        assert_eq!(s.sentence(), still_building(command));
        assert_eq!(s.reason.as_deref(), Some(STILL_BUILDING_REASON));
    }
}

#[test]
fn fix515_b664_a_command_whose_runner_may_run_tests_without_a_line_makes_no_claim() {
    for (command, tail) in [
        // `go test` prints nothing for a package until the package is done; with `-v` and several packages it holds each package's lines
        // back until that package ends.
        ("go test ./...", GO_DOWNLOADS),
        ("go test -v ./...", GO_DOWNLOADS),
        ("go build ./... && go test -v ./...", GO_DOWNLOADS),
        // Maven and Gradle run tests through plugins and tasks of any name.
        ("mvn -B test", MAVEN_BUILD),
        ("./mvnw -B verify", MAVEN_BUILD),
        (
            "./gradlew test",
            "> Task :compileJava\n> Task :compileTestJava\n",
        ),
        (
            "gradle check",
            "Downloading https://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
        ),
        // A redirect sends the runner's lines somewhere the report does not read.
        ("cargo test 2>&1", CARGO_BUILD),
        ("cargo test >/dev/null", CARGO_BUILD),
        // gotestsum prints its own lines.
        ("go run gotest.tools/gotestsum@v1.12.0 ./...", GO_DOWNLOADS),
        // Quiet Maven and Gradle leave out the lines that say the tests began.
        ("mvn -q test", MAVEN_BUILD),
        (
            "./gradlew --quiet test",
            "Downloading https://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
        ),
        // A pipe hands the runner's lines to another program.
        ("cargo test 2>&1 | tail -20", CARGO_BUILD),
        // A script, make, and a program started by cargo may run anything.
        ("make test", CARGO_BUILD),
        ("bash scripts/test-all.sh", MAVEN_BUILD),
        ("cargo run --bin checks", CARGO_BUILD),
        ("cargo test; npm test", CARGO_BUILD),
        // No program at all.
        ("CI=1", CARGO_BUILD),
        ("timeout 600 cargo test", CARGO_BUILD),
    ] {
        let s = summary(command, tail);
        assert!(!s.stopped_while_building(), "{command}: {s:?}");
        assert_eq!(s.sentence(), build_work(command), "{command}");
    }
}

/// The report keeps the end of a command's output. When the start was cut off, a line of a test that began may have been in the part that is
/// gone, so the claim is not made even for cargo.
#[test]
fn fix515_b664_an_output_whose_start_was_cut_off_makes_no_claim() {
    let mut step = timed_out("cargo test", CARGO_BUILD);
    step.output_truncated = true;
    let s = TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::TimedOut,
        reason: Some("a command was stopped at the report time limit".into()),
        started_at: None,
        duration_ms: 300_000,
        steps: vec![step],
        summary: None,
    }
    .summary();
    assert!(!s.stopped_while_building(), "{s:?}");
    assert_eq!(s.sentence(), build_work("cargo test"));
}

#[test]
fn fix515_b664_the_lines_that_maven_and_gradle_print_when_tests_begin_are_read() {
    for (command, tail) in [
        (
            "mvn -B test",
            "[INFO] Compiling 12 source files\n[INFO] -------------------------------------------------------\n[INFO]  T E S T S\n[INFO] -------------------------------------------------------\n[INFO] Running com.example.SlowIT\n",
        ),
        (
            "./mvnw -B verify",
            "[INFO] Compiling 3 source files\n[INFO] Running com.example.SlowIT\n",
        ),
        (
            "mvn -B test",
            "[INFO] Compiling 3 source files\n[ERROR] Tests run: 3, Failures: 1, Errors: 0, Skipped: 0\n",
        ),
        (
            "./gradlew test",
            "> Task :app:compileJava\nDownloading https://repo.maven.apache.org/maven2/a.jar\n> Task :app:test\n",
        ),
        (
            "cargo nextest run",
            "   Compiling a v0.1.0\n    Finished `test` profile [unoptimized] target(s) in 41.2s\n    Starting 5 tests across 1 binary\n",
        ),
    ] {
        let s = summary(command, tail);
        assert!(!s.stopped_while_building(), "{command}: {tail}");
        assert_eq!(s.sentence(), plain_timeout(command), "{tail}");
        // The worker's own reason stays.
        assert_eq!(
            s.reason.as_deref(),
            Some("a command was stopped at the report time limit")
        );
    }
}

#[test]
fn fix515_b664_the_fact_rides_in_the_reason_for_a_reader_of_the_summary_alone() {
    let s = summary("npm test", MOCHA);
    let j = serde_json::to_value(&s).unwrap();
    assert_eq!(j["reason"], BUILD_WORK);
    let back: TestReportSummary = serde_json::from_value(j).unwrap();
    assert_eq!(back.sentence(), s.sentence());
    // A report that ran to its end is never read this way, whatever its reason says.
    let mut ran = back.clone();
    ran.status = TestReportStatus::Ran;
    ran.exit_code = Some(1);
    assert!(!ran.stopped_while_building());
    assert!(!ran.sentence().contains("build work"), "{}", ran.sentence());
}
