//! The clean-checkout test report of a direct run.
//!
//! After Cline has ended and agentd has bundled the delivered head, agentd clones that bundle into a fresh directory inside the same
//! microVM, runs the repository's own test commands there and writes a [`TestReport`]: each command, its exit code, the counts the
//! runner printed and the end of its output. The report is an observation about commands the repository names. It never changes the
//! outcome label of a run and never blocks delivery, and it does not say the tests are meaningful.
//!
//! Who writes and reads what:
//!
//! - agentd (the worker) writes `runs/<run>/agents/direct/turns/1/test-report.json` ([`crate::direct::s3::test_report_key`]), the only
//!   place the worker role may write. The worker leaves `summary` out, and a reader never trusts one a worker wrote.
//! - run-session `plan-delivery` reads that object, checks it with [`TestReport::validate`], fills `summary` with
//!   [`TestReport::summary`], copies the result to `runs/<run>/evidence/test-report.json` ([`crate::direct::s3::evidence_test_report_key`],
//!   which the packager zips into `evidence/`) and puts the [`TestReportSummary`] in `DirectEvidence.testReport`.
//! - The packager, the pull request comment, `ham pull` and the lifecycle scorer (check LC-5) read the summary through
//!   [`TestReportSummary::claim`] and [`TestReportSummary::sentence`], so the one allowed wording lives in this file.
//!
//! Wording rule: "repository tests pass in a clean checkout: N of N" appears only when every setup and test
//! step exited 0, every test step printed counts the runner's parser recognised, nothing failed and at least one test passed; N is the
//! number of tests that passed (skipped tests are counted apart and never in N). No sentence here contains a word of
//! [`crate::direct::BANNED_CLAIM_WORDS`].
//!
//! Reading rule: the report is read leniently (an unknown member is ignored); [`TestReport::validate`] bounds every string and list.
//!
//! Protocol 1.41.0: the time one command may run is a setting of the user (`StartDirectRunRequest.reportStepS`, [`report_step_s_ok`]),
//! each command records the limit it was given ([`TestStep::limit_s`]), the summary names the command that was stopped and after how many seconds
//! ([`TimedOutCommand`]), and the worker tells the event stream what the report is doing with `turn.progress` events of kind [`PROGRESS_REPORT`]
//! ([`ReportProgress`]). The events change nothing about the report and never the outcome of the run.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `TestReport.schema_version`.
pub const TEST_REPORT_SCHEMA_VERSION: u32 = 1;
/// The time one command of the report may run when neither the user nor the stage says otherwise, in seconds (protocol 1.41.0). It is the
/// default of the stage variable `AGENTD_REPORT_STEP_S`.
pub const DEFAULT_REPORT_STEP_S: u32 = 300;
/// The longest time a user or a stage may give one command of the report, in seconds: as long as a session lives
/// ([`crate::direct::MAX_DIRECT_TIMEOUT_S`]). The report still ends at the run's own deadline, so a limit longer than the time left gives the
/// command what is left.
pub const MAX_REPORT_STEP_S: u32 = crate::direct::MAX_DIRECT_TIMEOUT_S;
/// Whether `s` seconds is a time limit the report accepts for one command: 1 to [`MAX_REPORT_STEP_S`]. `StartDirectRunRequest.reportStepS`
/// is checked with it by control-api and by agentd; `TestStep.limitS` is bounded by the same range.
pub fn report_step_s_ok(s: u32) -> bool {
    (1..=MAX_REPORT_STEP_S).contains(&s)
}
/// The start of the line agentd puts at the head of the `outputTail` of a `cargo test` step it chose from the manifest of a workspace, after
/// the `NOT RUN:` line when the step has one. [`plan_note_line`] is the whole line.
pub const PLAN_NOTE_PREFIX: &str = "PLAN NOTE: ";
/// The only command a plan note is about: what the planner detects for a Cargo workspace.
pub const PLAN_NOTE_COMMAND: &str = "cargo test";

/// The line agentd writes for a workspace of `members` members: `PLAN NOTE: cargo workspace, 14 members`.
pub fn plan_note_line(members: u32) -> String {
    format!("{PLAN_NOTE_PREFIX}cargo workspace, {members} members")
}

/// The start of a head line of a test step's `outputTail` that names a service the step ran without: `NEEDS: postgres: <why it was not
/// available>`. agentd writes it, after the `NOT RUN:` line and the plan note when the step has them, and only for a test step that exited with a
/// code other than 0 while a service the repository declares was not reachable and the step's own output said it could not connect to it. The step
/// keeps its exit code, its counts and its output; the line is a reading of them. [`TestStep::needs`] reads it.
pub const NEEDS_PREFIX: &str = "NEEDS: ";
/// The start of the head line of the `outputTail` of a service step: `STAND-IN: <sentence>`. A service step is a setup step whose command starts
/// with [`SERVICE_COMMAND_PREFIX`]; its exit code says whether the report started the local server it names, and the sentence says which declared
/// Compose service the server stands in for and that the Compose container itself did not run. [`TestStep::stand_in`] reads it.
pub const STAND_IN_PREFIX: &str = "STAND-IN: ";
/// The start of the command of a service step: the image's helper that starts a local PostgreSQL or RabbitMQ in the session.
pub const SERVICE_COMMAND_PREFIX: &str = "ham-services ";
/// The most lines `TestReportSummary.details` holds.
pub const MAX_SUMMARY_DETAILS: usize = 16;
/// The most characters of one line of `TestReportSummary.details`.
pub const MAX_SUMMARY_DETAIL_CHARS: usize = 300;

/// The client setting that gives one command of the report its time limit, in whole minutes; `ham` sends it as `reportStepS`.
pub const REPORT_LIMIT_SETTING: &str = "run.report_command_minutes";
/// The stage variable that sets the limit of one command when a run sets none; [`DEFAULT_REPORT_STEP_S`] is its default.
pub const REPORT_LIMIT_VARIABLE: &str = "AGENTD_REPORT_STEP_S";
/// Steps in one report: setup and test steps together.
pub const MAX_REPORT_STEPS: usize = 32;
/// The most bytes of output a step may carry: the ceiling that [`TestReport::validate`] accepts and the stage variable `AGENTD_REPORT_TAIL_KIB`
/// may reach. The end of the output is the part kept.
pub const MAX_REPORT_TAIL_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of output agentd keeps per step unless the stage says otherwise (`AGENTD_REPORT_TAIL_KIB`): 256 KiB, enough for the whole output of
/// almost every suite.
pub const DEFAULT_REPORT_TAIL_BYTES: usize = 256 * 1024;
/// Characters of a command line kept.
pub const MAX_REPORT_COMMAND_CHARS: usize = 2_000;
/// Characters of a `reason`.
pub const MAX_REPORT_REASON_CHARS: usize = 1_000;
/// The largest serialized report: 16 MiB. The report is one S3 object that run-session (a Lambda function with 256 MB) reads, checks and copies,
/// so this is the bound of the readers' memory, not of what the user may read: a report over it is shortened by [`TestReport::bounded`], longest
/// tail first, until it fits.
pub const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
/// The `reason` of the `not_run` report agentd writes when too little time is left before the run's deadline. run-session puts it in the detail
/// of `run.completed` word for word, so a skipped report is said in the run's events.
pub const REPORT_SKIPPED_NO_TIME: &str =
    "test report skipped: not enough time before the run deadline";
/// Characters of the runner name in `countsFrom`.
pub const MAX_REPORT_RUNNER_CHARS: usize = 40;
/// `TestStep.counts_from` of a step agentd did not run, or whose result its environment decided (a command that needs a container runtime the
/// session does not have). Only agentd sets it; the output of a command cannot, so a repository cannot mark its own failing step as not run.
pub const NOT_RUN_MARK: &str = "not run";
/// The start of the `outputTail` of a step with [`NOT_RUN_MARK`]: the reason follows, then, for a command that was started, what it printed.
pub const NOT_RUN_PREFIX: &str = "NOT RUN: ";

/// How the report ended. `off` is never written by the worker: run-session puts it in a summary when the user switched the report off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestReportStatus {
    /// Every planned step ran to its end, or a setup step failed and the steps after it were not started. A step may have failed.
    Ran,
    /// The repository names no test command and none could be detected.
    NoCommand,
    /// The report could not be made (the checkout failed, for example); `reason` says why.
    NotRun,
    /// The time limit ended the steps; the steps that finished are listed.
    TimedOut,
    /// The user switched the report off for this run (`reportTests: false`).
    Off,
}

/// What a step is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestStepRole {
    /// Installs what the test step needs (a virtual environment, `npm ci`). Not counted.
    Setup,
    /// A test command whose counts and exit code decide whether the wording may appear.
    Test,
}

/// Where a command came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestCommandSource {
    /// A line of `.ham/test-command` in the delivered tree.
    TestCommandFile,
    /// A `RUN` line of `docs/lifecycle/LOG.md`.
    LifecycleLog,
    /// A numbered check of `docs/lifecycle/GOAL.md`.
    GoalCard,
    /// A manifest of the tree (`pyproject.toml`, `package.json`, `go.mod`, ...).
    Detected,
}

/// What a runner printed. `failed` includes errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestCounts {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// What the lifecycle log said about the same command: `RUN | <command> | exit <code> | pass <n or unknown> fail <n or unknown> | ...`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoggedRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
}

/// One command of the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestStep {
    pub role: TestStepRole,
    pub source: TestCommandSource,
    /// At most [`MAX_REPORT_COMMAND_CHARS`] characters.
    pub command: String,
    /// `None` when the command was stopped (time limit) or never started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The time limit agentd gave this command, in whole seconds rounded up: the user's or the stage's step limit, or
    /// less when the rest of the report's budget was shorter. `None` for a command never started and for a report an older agentd wrote;
    /// 1 to [`MAX_REPORT_STEP_S`] otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_s: Option<u32>,
    /// The counts the runner's summary line gave; `None` when no parser recognised the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<TestCounts>,
    /// The runner the counts were read from (`pytest`, `cargo`, ...), at most [`MAX_REPORT_RUNNER_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts_from: Option<String>,
    /// The end of the output (standard output and error together), at most [`MAX_REPORT_TAIL_BYTES`] bytes, secrets redacted.
    #[serde(default)]
    pub output_tail: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_truncated: bool,
    /// For a step that reruns a command of the lifecycle log: what the log said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged: Option<LoggedRun>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl TestStep {
    /// The member count of the plan note this step carries: `Some(n)` when the step is a test step the harness chose from the manifest
    /// (source `Detected`), its command is `cargo test` and the head of its output is [`plan_note_line`] for an `n` of at least 2, after the `NOT RUN:`
    /// line when agentd marked the step as not run. agentd writes the line and removes any like it from the repository's output, and no other step is
    /// read, so the output of a command cannot make a note for a command the repository named.
    pub fn plan_note_members(&self) -> Option<u32> {
        if self.role != TestStepRole::Test
            || self.source != TestCommandSource::Detected
            || self.command != PLAN_NOTE_COMMAND
        {
            return None;
        }
        let mut lines = self.output_tail.lines();
        let first = lines.next()?;
        let line = if self.is_not_run() && first.starts_with(NOT_RUN_PREFIX) {
            lines.next()?
        } else {
            first
        };
        let digits = line
            .strip_prefix(PLAN_NOTE_PREFIX)?
            .strip_prefix("cargo workspace, ")?
            .strip_suffix(" members")?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<u32>().ok().filter(|n| *n >= 2)
    }

    /// The head lines of the output after the `NOT RUN:` line and the plan note, which are agentd's own.
    fn head_lines(&self) -> impl Iterator<Item = &str> {
        let mut lines = self.output_tail.lines().peekable();
        if self.is_not_run() && lines.peek().is_some_and(|l| l.starts_with(NOT_RUN_PREFIX)) {
            lines.next();
        }
        while lines
            .peek()
            .is_some_and(|l| l.starts_with(PLAN_NOTE_PREFIX))
        {
            lines.next();
        }
        lines
    }

    /// The services this step ran without, as `(name, why)`: the `NEEDS:` lines at the head of the output of a test step that ran and
    /// exited with a code other than 0. A step that exited 0, one that was not run, a timed-out step and a setup step have none, so the output of a
    /// command cannot add a need to a step that passed, and a repository that prints such a line in the middle of its output adds nothing.
    pub fn needs(&self) -> Vec<(&str, &str)> {
        if self.role != TestStepRole::Test
            || self.is_not_run()
            || self.timed_out
            || self.exit_code.is_none_or(|c| c == 0)
        {
            return Vec::new();
        }
        self.head_lines()
            .map_while(|l| l.strip_prefix(NEEDS_PREFIX))
            .filter_map(|rest| rest.split_once(": "))
            .collect()
    }

    /// The sentence of a service step: the `STAND-IN:` line at the head of the output of a setup step whose command starts with
    /// [`SERVICE_COMMAND_PREFIX`].
    pub fn stand_in(&self) -> Option<&str> {
        if self.role != TestStepRole::Setup || !self.command.starts_with(SERVICE_COMMAND_PREFIX) {
            return None;
        }
        self.head_lines()
            .find_map(|l| l.strip_prefix(STAND_IN_PREFIX))
    }

    /// A step that was not run: no exit code, no counts, and `NOT RUN: <reason>` in the output.
    pub fn not_run(
        role: TestStepRole,
        source: TestCommandSource,
        command: &str,
        reason: &str,
        logged: Option<LoggedRun>,
    ) -> Self {
        Self {
            role,
            source,
            command: command.to_string(),
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            limit_s: None,
            counts: None,
            counts_from: Some(NOT_RUN_MARK.to_string()),
            output_tail: format!("{NOT_RUN_PREFIX}{reason}"),
            output_truncated: false,
            logged,
        }
    }

    /// Whether agentd marked this step as not run ([`NOT_RUN_MARK`]). Such a step is neither a pass nor a failure.
    pub fn is_not_run(&self) -> bool {
        !self.timed_out && self.counts_from.as_deref() == Some(NOT_RUN_MARK)
    }

    /// The reason of a step that was not run: the first line of its output after [`NOT_RUN_PREFIX`].
    pub fn not_run_reason(&self) -> Option<&str> {
        let rest = self.output_tail.strip_prefix(NOT_RUN_PREFIX)?;
        Some(rest.lines().next().unwrap_or(rest))
    }
}

/// `test-report.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestReport {
    /// [`TEST_REPORT_SCHEMA_VERSION`].
    pub schema_version: u32,
    pub run_id: String,
    /// The delivered head the clean checkout was made at.
    pub head_sha: String,
    pub status: TestReportStatus,
    /// Why the report is `no_command`, `not_run` or `timed_out`; at most [`MAX_REPORT_REASON_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub duration_ms: u64,
    pub steps: Vec<TestStep>,
    /// Filled by run-session from the steps ([`TestReport::summary`]); the worker leaves it out and a reader never trusts a worker's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<TestReportSummary>,
}

/// How the reason of a `no_command` report starts when it lists where the report looked: the card sentence carries such a reason, and an older
/// reason that only repeats that no command was found is left out of it.
pub const NO_COMMAND_LOOKED_FOR: &str = "it looked for ";
/// How the reason of a `no_command` report starts when the report found test files and did not run them: a plain Python layout whose
/// packages are named by a file the report has no install step for. The card sentence then says that the tests were not run, and the reason.
pub const NO_COMMAND_FOUND_TESTS: &str = "it found ";

/// The first line of a summary's `reason` when the command that was stopped at its limit was still building: the end of its output held a
/// line that a build or a package download prints and no line of a test that began ([`stopped_while_building`]), every program the command
/// starts is a runner that prints such a line when its tests begin ([`announces_tests`]), and the report kept the whole output of the command
/// (`outputTruncated` is false), so the missing line shows that no test began. The summary carries the fact in this member, which every reader
/// of the summary has, so EVIDENCE.md, the pull request and `ham pull` say the same.
pub const STILL_BUILDING_REASON: &str =
    "the command was still building when it was stopped: its output shows no test that began";

/// The first line of a summary's `reason` when the end of the stopped command's output held a build line and no test line the reading knows, and
/// that does not show that no test began: the command starts a program whose tests may begin without such a line (mocha's check marks, the dots
/// of `pytest -q`, the go tool, which holds a package's lines back, a Maven plugin, a script), or the start of the output was cut off. The output
/// shows build work, and whether a test began is not known, so the sentence does not say that none did.
pub const BUILD_WORK_REASON: &str = "the command was stopped while the end of its output showed build work: whether a test began is not known";

/// The starts of the lines a build or a package download prints before any test runs: cargo, the go tool, Maven and Gradle.
const BUILD_LINE_STARTS: &[&str] = &[
    "Compiling ",
    "Checking ",
    "Building ",
    "Downloading ",
    "Downloaded ",
    "Updating ",
    "Locking ",
    "Adding ",
    "Blocking waiting for file lock",
    "Documenting ",
    "Fetching ",
    "go: downloading ",
    "go: finding ",
    "go: extracting ",
    "[INFO] Download",
    "[INFO] Compiling ",
    "[INFO] Building ",
    "> Task :compile",
];

/// The starts of the lines a test runner prints once its tests begin, and cargo's line that the build is over. Maven's lines are read with and
/// without the level it puts in front (`[INFO]  T E S T S`, `[INFO] Running com.example.FooTest`, `[ERROR] Tests run: 3, Failures: 1`).
const TEST_LINE_STARTS: &[&str] = &[
    "Finished ",
    "Running ",
    "Doc-tests ",
    "running ",
    "test result:",
    "test ",
    "=== RUN",
    "--- PASS",
    "--- FAIL",
    "ok ",
    "FAIL",
    "PASS",
    "collected ",
    "T E S T S",
    "Tests run:",
    "> Task :test",
];

/// The levels Maven puts in front of a line.
const MAVEN_LEVELS: [&str; 3] = ["[INFO] ", "[WARNING] ", "[ERROR] "];

/// Whether `l`, a line without its leading spaces, is one a runner prints once its tests begin: a start of [`TEST_LINE_STARTS`] (after Maven's
/// level, when the line has one), pytest's `test session starts`, or the start of a Gradle test task of any project (`> Task :app:test`).
fn is_test_line(l: &str) -> bool {
    let core = MAVEN_LEVELS
        .iter()
        .find_map(|level| l.strip_prefix(level))
        .map_or(l, str::trim_start);
    TEST_LINE_STARTS.iter().any(|p| core.starts_with(p))
        || l.contains("test session starts")
        || (l.starts_with("> Task :") && l.contains(":test"))
}

/// Whether `l`, a line without its leading spaces, is one a build or a package download prints: a start of [`BUILD_LINE_STARTS`], or the start of
/// a Gradle compile task of any project (`> Task :app:compileJava`).
fn is_build_line(l: &str) -> bool {
    BUILD_LINE_STARTS.iter().any(|p| l.starts_with(p))
        || (l.starts_with("> Task :") && l.contains(":compile"))
}

/// Whether the end of a stopped command's output shows build work and no test: at least one build line ([`BUILD_LINE_STARTS`]) and no line
/// of a test that began ([`TEST_LINE_STARTS`], or pytest's `test session starts`). An empty or unknown output is not called building. The lines
/// are the ones this reading knows: whether their absence shows that no test began depends on the command ([`announces_tests`]).
pub fn stopped_while_building(tail: &str) -> bool {
    let mut build = false;
    for line in tail.lines() {
        let l = line.trim_start();
        if is_test_line(l) {
            return false;
        }
        build |= is_build_line(l);
    }
    build
}

/// Whether every program `command` starts is a runner that always prints a line this reading knows before its first test runs, so that a whole
/// output with build lines and none of those lines shows that no test began. That runner is cargo: `cargo test`, `cargo nextest` and
/// cargo's build commands, after a `+toolchain` word when there is one. Cargo builds everything first and prints `Finished` and `Running`
/// before it starts a test binary, and the test binary prints `running N tests`.
///
/// No other runner is taken: `go test` prints nothing for a package until the package is done and, for several packages, holds the lines of
/// `-v` back until then; Maven and Gradle run tests through plugins and tasks of any name; a script, `make`, `npm`, `pytest` or `timeout` may
/// start any program. The segments of `&&`, `||` and `;` are read one by one, a `cd` and `NAME=value` prefixes are passed over, and the
/// program is known by the last part of its path. A pipe hands the lines to another program and a redirect sends them where the report does
/// not read, so both give `false`, and so does a command with no program.
pub fn announces_tests(command: &str) -> bool {
    let mut any = false;
    let segments = command
        .split("&&")
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split(';'));
    for segment in segments {
        if segment.contains(['|', '>', '<']) {
            return false;
        }
        let mut words = segment.split_whitespace().skip_while(|w| {
            w.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        });
        let Some(program) = words.next() else {
            continue;
        };
        let known = match program.rsplit('/').next().unwrap_or(program) {
            "cd" => continue,
            "cargo" => matches!(
                words.find(|w| !w.starts_with('+')),
                Some("test" | "nextest" | "build" | "check" | "clippy" | "fetch")
            ),
            _ => false,
        };
        if !known {
            return false;
        }
        any = true;
    }
    any
}

/// The command a report stopped at its time limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimedOutCommand {
    /// At most [`MAX_REPORT_COMMAND_CHARS`] characters.
    pub command: String,
    /// The limit the command was given, in whole seconds: [`TestStep::limit_s`] when the worker recorded it, else the time the command ran,
    /// to the nearest second. Never 0.
    pub after_s: u32,
}

/// What a reader needs without the steps: carried in `DirectEvidence.testReport`, in the pull request comment and by `ham pull`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestReportSummary {
    pub status: TestReportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Setup and test steps that ran to an exit.
    pub steps_run: u32,
    /// Every setup and test step exited 0 and none timed out, and at least one test step exists.
    pub all_exit_zero: bool,
    /// The first exit code that was not 0 among the setup and test steps; else the exit code of the last test step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A setup step failed, so no test step ran after it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub setup_failed: bool,
    /// The command of the first setup or test step that did not exit 0 (or timed out), at most [`MAX_REPORT_COMMAND_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_command: Option<String>,
    /// The first command that was stopped at its time limit, with the seconds it was given. `None` when no command
    /// timed out, also for a report whose time ran out between two commands. Computed from the steps by every reader, so an older report
    /// has it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<TimedOutCommand>,
    /// The counts of the test steps, present only when every test step has counts.
    /// Steps of one runner stand for the largest of each count among them. The counts of different runners are added, and runners that report the same counts count once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<TestCounts>,
    /// The first test command, at most [`MAX_REPORT_COMMAND_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub duration_ms: u64,
    /// The services the first failing step ran without ([`TestStep::needs`]); empty for every other report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// The services the report started in place of the Compose services the repository declares, by name (`postgres`): the service steps that
    /// exited 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services_started: Vec<String>,
    /// The lines EVIDENCE.md prints under the sentence, one fact each, derived from the steps: what each service step did and what each test
    /// step did and needed. At most [`MAX_SUMMARY_DETAILS`] lines of [`MAX_SUMMARY_DETAIL_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

impl TestReport {
    /// A report that checked nothing: the status and the reason alone.
    pub fn without_steps(
        run_id: &str,
        head_sha: &str,
        status: TestReportStatus,
        reason: Option<String>,
    ) -> Self {
        Self {
            schema_version: TEST_REPORT_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            head_sha: head_sha.to_string(),
            status,
            reason,
            started_at: None,
            duration_ms: 0,
            steps: Vec::new(),
            summary: None,
        }
    }

    /// The summary of the steps. Pure: the same steps always give the same summary, whatever `summary` holds.
    ///
    /// Counts are present only when every test step has counts.
    /// Steps are grouped by runner name, with unnamed steps forming a separate group.
    /// Each group's passed, failed, and skipped counts are the largest corresponding values reported by its steps.
    /// Group counts are added, with identical triples included once.
    /// Step order does not affect the result.
    /// The result is bounded by the largest step and the sum of all steps.
    ///
    /// A step marked not run ([`TestStep::is_not_run`]) is neither a pass nor a failure: it does not count as run, it is not the failing
    /// command, it gives no exit code and it fails no setup, and it keeps `all_exit_zero` (and so the claim) away, because a command that
    /// did not run shows nothing. The summary's `reason` then names the first such command and why (`NOT RUN: ...`) when the report has none.
    pub fn summary(&self) -> TestReportSummary {
        let ran: Vec<&TestStep> = self.steps.iter().filter(|s| !s.is_not_run()).collect();
        let not_run: Vec<&TestStep> = self.steps.iter().filter(|s| s.is_not_run()).collect();
        let tests: Vec<&TestStep> = ran
            .iter()
            .copied()
            .filter(|s| s.role == TestStepRole::Test)
            .collect();
        let zero = |s: &TestStep| s.exit_code == Some(0) && !s.timed_out;
        let first_failing = ran.iter().copied().find(|s| !zero(s));
        let exit_code = ran
            .iter()
            .find_map(|s| s.exit_code.filter(|c| *c != 0))
            .or_else(|| tests.last().and_then(|s| s.exit_code));
        let all_tests_counted = !tests.is_empty() && tests.iter().all(|s| s.counts.is_some());
        let counts = all_tests_counted.then(|| combine_counts(&tests));
        // A command stopped while the end of its output showed build work gives the report its reason: the worker's "a command was
        // stopped" says less. That no test began is said only for a command whose runners announce their tests, and only when the report
        // kept the whole output: a line of a test that began may have been in a start that was cut off.
        let own = match ran.iter().find(|s| s.timed_out) {
            Some(s) if stopped_while_building(&s.output_tail) => Some(
                if announces_tests(&s.command) && !s.output_truncated {
                    STILL_BUILDING_REASON
                } else {
                    BUILD_WORK_REASON
                }
                .to_string(),
            ),
            _ => self.reason.clone(),
        };
        let reason = own.or_else(|| {
            let first = not_run.first()?;
            let why = cut_chars(
                first.not_run_reason().unwrap_or("a service is missing"),
                120,
            );
            let more = match not_run.len() {
                1 => String::new(),
                n => format!(", and {} more", n - 1),
            };
            Some(format!(
                "{NOT_RUN_PREFIX}`{}` ({why}){more}",
                quoted(&cut_chars(&first.command, MAX_REPORT_COMMAND_CHARS))
            ))
        });
        // The plan note is the second line of the reason: the first line stays the report's own reason or the not-run reason, so a note never
        // hides either.
        let reason = match self.steps.iter().find_map(TestStep::plan_note_members) {
            Some(n) => Some(format!(
                "{}\n{}",
                reason.unwrap_or_default(),
                plan_note_text(n)
            )),
            None => reason,
        };
        let timed_out = ran.iter().find(|s| s.timed_out).map(|s| TimedOutCommand {
            command: cut_chars(&s.command, MAX_REPORT_COMMAND_CHARS),
            after_s: s
                .limit_s
                .unwrap_or_else(|| {
                    u32::try_from(s.duration_ms.saturating_add(500) / 1000).unwrap_or(u32::MAX)
                })
                .max(1),
        });
        let needs: Vec<String> = first_failing
            .map(|s| s.needs().into_iter().map(|(n, _)| n.to_string()).collect())
            .unwrap_or_default();
        let services_started: Vec<String> = ran
            .iter()
            .filter(|s| s.stand_in().is_some() && zero(s))
            .filter_map(|s| s.command.strip_prefix(SERVICE_COMMAND_PREFIX))
            .map(|n| n.split_whitespace().next().unwrap_or(n).to_string())
            .collect();
        TestReportSummary {
            status: self.status,
            reason,
            steps_run: u32::try_from(ran.iter().filter(|s| s.exit_code.is_some()).count())
                .unwrap_or(u32::MAX),
            all_exit_zero: !tests.is_empty() && ran.iter().all(|s| zero(s)) && not_run.is_empty(),
            exit_code,
            setup_failed: ran
                .iter()
                .any(|s| s.role == TestStepRole::Setup && !zero(s)),
            failed_command: first_failing.map(|s| cut_chars(&s.command, MAX_REPORT_COMMAND_CHARS)),
            timed_out,
            counts,
            command: tests
                .first()
                .map(|s| cut_chars(&s.command, MAX_REPORT_COMMAND_CHARS)),
            duration_ms: self.duration_ms,
            needs,
            services_started,
            details: self.step_details(),
        }
    }

    /// One plain line for each service step and each test step: what it did, with its exit code and counts as the runner gave them, and what it
    /// ran without. Setup steps other than service steps have no line.
    fn step_details(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.steps {
            let line = if let Some(sentence) = s.stand_in() {
                match (s.is_not_run(), s.exit_code) {
                    (false, Some(0)) => sentence.to_string(),
                    (true, _) => format!(
                        "`{}` was not run ({}). {sentence}",
                        quoted(&s.command),
                        cut_chars(s.not_run_reason().unwrap_or("it did not start"), 120)
                    ),
                    (false, code) => format!(
                        "`{}` exited {}. {sentence}",
                        quoted(&s.command),
                        code.map_or_else(|| "with no code".to_string(), |c| c.to_string())
                    ),
                }
            } else if s.role == TestStepRole::Test {
                let result = if s.is_not_run() {
                    format!(
                        "not run ({})",
                        cut_chars(s.not_run_reason().unwrap_or("it was not run"), 120)
                    )
                } else if s.timed_out {
                    format!("timed out after {} s", s.limit_s.unwrap_or(0).max(1))
                } else {
                    let code = s
                        .exit_code
                        .map_or_else(|| "no code".to_string(), |c| format!("exit {c}"));
                    match s.counts {
                        Some(c) => format!("{code}, {} passed, {} failed", c.passed, c.failed),
                        None => format!("{code}, counts not read"),
                    }
                };
                let ran_without = s.needs();
                if ran_without.is_empty() {
                    format!("`{}`: {result}.", quoted(&s.command))
                } else {
                    let list: Vec<String> = ran_without
                        .iter()
                        .map(|(n, why)| format!("{n} ({})", cut_chars(why, 100)))
                        .collect();
                    format!(
                        "`{}`: {result}; it ran without {}.",
                        quoted(&s.command),
                        list.join(", ")
                    )
                }
            } else {
                continue;
            };
            out.push(cut_chars(&plain(&line), MAX_SUMMARY_DETAIL_CHARS));
            if out.len() == MAX_SUMMARY_DETAILS {
                break;
            }
        }
        out
    }

    /// Whether this report is about `run_id` at `head_sha` and within every bound; the error names the first rule broken.
    pub fn validate(&self, run_id: &str, head_sha: &str) -> Result<(), String> {
        if self.schema_version != TEST_REPORT_SCHEMA_VERSION {
            return Err(format!(
                "schemaVersion must be {TEST_REPORT_SCHEMA_VERSION}"
            ));
        }
        if self.run_id != run_id {
            return Err("runId differs from the run".into());
        }
        if self.head_sha != head_sha {
            return Err("headSha differs from the delivered head".into());
        }
        if self.status == TestReportStatus::Off {
            return Err("status off is set by run-session, never by the worker".into());
        }
        if self.steps.len() > MAX_REPORT_STEPS {
            return Err(format!("steps: at most {MAX_REPORT_STEPS}"));
        }
        let status_fits = match self.status {
            TestReportStatus::NoCommand | TestReportStatus::NotRun => self.steps.is_empty(),
            TestReportStatus::Ran | TestReportStatus::TimedOut => !self.steps.is_empty(),
            TestReportStatus::Off => false,
        };
        if !status_fits {
            return Err("status does not fit the steps".into());
        }
        if let Some(r) = &self.reason {
            if r.chars().count() > MAX_REPORT_REASON_CHARS {
                return Err(format!(
                    "reason is longer than {MAX_REPORT_REASON_CHARS} characters"
                ));
            }
            if r.chars().any(char::is_control) {
                return Err("control character in reason".into());
            }
        }
        if let Some(a) = &self.started_at
            && (a.chars().count() > 40 || a.chars().any(char::is_control))
        {
            return Err("startedAt is not a short plain time".into());
        }
        for s in &self.steps {
            if s.command.chars().count() > MAX_REPORT_COMMAND_CHARS {
                return Err(format!(
                    "command is longer than {MAX_REPORT_COMMAND_CHARS} characters"
                ));
            }
            if s.command.chars().any(char::is_control) {
                return Err("control character in command".into());
            }
            if s.output_tail.len() > MAX_REPORT_TAIL_BYTES {
                return Err(format!(
                    "outputTail is longer than {MAX_REPORT_TAIL_BYTES} bytes"
                ));
            }
            if s.output_tail
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err("control character in outputTail".into());
            }
            if s.counts_from.as_deref().is_some_and(|r| {
                r.chars().count() > MAX_REPORT_RUNNER_CHARS || r.chars().any(char::is_control)
            }) {
                return Err("countsFrom is not a short plain runner name".into());
            }
            if s.timed_out && s.exit_code.is_some() {
                return Err("timedOut step has an exit code".into());
            }
            if s.limit_s.is_some_and(|l| !report_step_s_ok(l)) {
                return Err(format!("limitS must be 1 to {MAX_REPORT_STEP_S}"));
            }
        }
        match serde_json::to_vec(self) {
            Ok(b) if b.len() <= MAX_REPORT_BYTES => Ok(()),
            _ => Err(format!("report is larger than {MAX_REPORT_BYTES} bytes")),
        }
    }

    /// The report cut to its bounds: strings shortened at a character boundary, control characters other than newline and tab removed,
    /// extra steps dropped and, when the serialized report is still over [`MAX_REPORT_BYTES`] (output that JSON writes in two bytes a
    /// character, such as newlines, quotes and backslashes), the longest output tail halved again and again until it fits, each cut tail
    /// marked `outputTruncated`. The end of every output is the part kept. `validate` passes on the result for the same run and head.
    pub fn bounded(mut self) -> Self {
        self.steps.truncate(MAX_REPORT_STEPS);
        for s in &mut self.steps {
            s.command = cut_chars(&plain(&s.command), MAX_REPORT_COMMAND_CHARS);
            s.counts_from = s
                .counts_from
                .take()
                .map(|r| cut_chars(&plain(&r), MAX_REPORT_RUNNER_CHARS));
            let cleaned: String = s
                .output_tail
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect();
            let (tail, cut) = tail_bytes(&cleaned, MAX_REPORT_TAIL_BYTES);
            s.output_tail = tail;
            s.output_truncated |= cut;
        }
        self.reason = self
            .reason
            .take()
            .map(|r| cut_chars(&plain(&r), MAX_REPORT_REASON_CHARS));
        self.started_at = self.started_at.take().map(|a| cut_chars(&plain(&a), 40));
        self.summary = None;
        // The serialized size, worked out once; after that only the step that was cut is measured again. Every field is bounded, so with no
        // tail at all the report is far below the limit and the loop ends; a report that cannot be serialized is left to `validate`, which
        // refuses it.
        let Ok(mut total) = serde_json::to_vec(&self).map(|b| b.len()) else {
            return self;
        };
        while total > MAX_REPORT_BYTES {
            // The longest tail first, the earliest step among equals: the last step is the test run and the most worth keeping.
            let Some(i) = self
                .steps
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.output_tail.is_empty())
                .min_by_key(|(_, s)| std::cmp::Reverse(s.output_tail.len()))
                .map(|(i, _)| i)
            else {
                break;
            };
            let size = |s: &TestStep| serde_json::to_vec(s).map_or(0, |b| b.len());
            let before = size(&self.steps[i]);
            let half = self.steps[i].output_tail.len() / 2;
            self.steps[i].output_tail = tail_bytes(&self.steps[i].output_tail, half).0;
            self.steps[i].output_truncated = true;
            total = total.saturating_sub(before) + size(&self.steps[i]);
        }
        self
    }
}

/// `s` without control characters.
fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// The first `max` characters of `s`.
fn cut_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The counts of test steps that all have counts, as [`TestReport::summary`] states: the largest of each count among the steps of one runner, the groups
/// added, and groups with exactly the same counts taken once (two runners that report the same counts ran the same tests, as `pytest` does for a `unittest`
/// suite). A count is never more than the sum of the steps and never less than the largest step, so it never exceeds the tests that exist.
fn combine_counts(tests: &[&TestStep]) -> TestCounts {
    let mut groups: Vec<(Option<&str>, TestCounts)> = Vec::new();
    for step in tests {
        let Some(c) = step.counts else { continue };
        let runner = step.counts_from.as_deref();
        match groups.iter_mut().find(|(r, _)| *r == runner) {
            Some((_, g)) => {
                g.passed = g.passed.max(c.passed);
                g.failed = g.failed.max(c.failed);
                g.skipped = g.skipped.max(c.skipped);
            }
            None => groups.push((runner, c)),
        }
    }
    let mut distinct: Vec<TestCounts> = Vec::new();
    for (_, c) in groups {
        if !distinct.contains(&c) {
            distinct.push(c);
        }
    }
    distinct
        .into_iter()
        .fold(TestCounts::default(), |acc, c| TestCounts {
            passed: acc.passed.saturating_add(c.passed),
            failed: acc.failed.saturating_add(c.failed),
            skipped: acc.skipped.saturating_add(c.skipped),
        })
}

/// The last `max` bytes of `s`, cut at a character boundary, and whether anything was cut.
fn tail_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_string(), true)
}

/// The note for a `cargo test` the harness chose from the manifest of a workspace of `members` members.
fn plan_note_text(members: u32) -> String {
    format!(
        "The repository names no test command, so the harness chose `{PLAN_NOTE_COMMAND}` from the manifest (Cargo.toml). It runs the tests of the whole workspace, {members} members. A `.ham/test-command` file in the repository names a narrower command."
    )
}

/// A command as it stands inside backticks in a sentence: a backtick of its own becomes an apostrophe.
fn quoted(command: &str) -> String {
    plain(command).replace('`', "'")
}

impl TestReportSummary {
    /// The first line of `reason`: why the report ended as it did, or the first command that was not run. The second line, if any, is the plan note.
    fn why(&self) -> Option<&str> {
        self.reason
            .as_deref()
            .and_then(|r| r.lines().next())
            .filter(|l| !l.is_empty())
    }

    /// The note EVIDENCE.md adds for a command the harness chose from the manifest of a Cargo workspace: the second line of `reason`, which
    /// [`TestReport::summary`] writes from [`TestStep::plan_note_members`]. `None` for any other report, also for a summary an older build wrote.
    /// [`TestReportSummary::sentence`] does not carry it.
    pub fn plan_note(&self) -> Option<&str> {
        self.reason
            .as_deref()
            .and_then(|r| r.split_once('\n'))
            .map(|(_, note)| note)
            .filter(|n| !n.is_empty())
    }

    /// The note EVIDENCE.md adds for a command that was stopped at its time limit: the command, the limit it was given in seconds and
    /// the two settings that change it, the client setting `run.report_command_minutes` and the stage variable `AGENTD_REPORT_STEP_S`. `None`
    /// when no command was stopped, also when the report's whole time ran out between two commands (the per-command setting is not the cause then).
    /// [`TestReportSummary::sentence`] does not carry it: `ham` adds its own advice to the sentence, and the setting would show twice.
    pub fn limit_note(&self) -> Option<String> {
        let t = self.timed_out.as_ref()?;
        Some(format!(
            "`{}` was stopped at the limit of {} s. The limit for one command is the client setting `{REPORT_LIMIT_SETTING}` (`ham config set {REPORT_LIMIT_SETTING} <minutes>`). When a run sets none, the stage's own limit applies; the stage variable `{REPORT_LIMIT_VARIABLE}` sets it.",
            quoted(&t.command),
            t.after_s
        ))
    }

    /// The command stopped at its limit was still building, so no test had started: the first line of `reason` is [`STILL_BUILDING_REASON`].
    pub fn stopped_while_building(&self) -> bool {
        self.status == TestReportStatus::TimedOut && self.why() == Some(STILL_BUILDING_REASON)
    }

    /// The end of the output of the command stopped at its limit showed build work, and whether a test began is not known: the first
    /// line of `reason` is [`BUILD_WORK_REASON`].
    pub fn stopped_in_build_work(&self) -> bool {
        self.status == TestReportStatus::TimedOut && self.why() == Some(BUILD_WORK_REASON)
    }

    /// The summary of a report the user switched off.
    pub fn off() -> Self {
        Self {
            status: TestReportStatus::Off,
            reason: None,
            steps_run: 0,
            all_exit_zero: false,
            exit_code: None,
            setup_failed: false,
            failed_command: None,
            timed_out: None,
            counts: None,
            command: None,
            duration_ms: 0,
            needs: Vec::new(),
            services_started: Vec::new(),
            details: Vec::new(),
        }
    }

    /// "repository tests pass in a clean checkout: N of N", or `None` when the evidence does not allow it. N is the number of tests that
    /// passed; the rule is in the module documentation.
    pub fn claim(&self) -> Option<String> {
        if self.status != TestReportStatus::Ran || !self.all_exit_zero || self.setup_failed {
            return None;
        }
        let c = self.counts?;
        (c.failed == 0 && c.passed > 0).then(|| {
            format!(
                "repository tests pass in a clean checkout: {n} of {n}",
                n = c.passed
            )
        })
    }

    /// One plain sentence for EVIDENCE.md, the pull request and `ham pull`: the claim when it is allowed, else what happened.
    pub fn sentence(&self) -> String {
        match self.status {
            TestReportStatus::Off => {
                return "The clean-checkout test report was switched off for this run.".into();
            }
            // What the report looked for comes after the sentence when the report says it.
            TestReportStatus::NoCommand => {
                return match self.why() {
                    // Test files were found and not run: the sentence says that, with the reason.
                    Some(r) if r.starts_with(NO_COMMAND_FOUND_TESTS) => {
                        format!("The clean-checkout test report did not run the tests: {r}.")
                    }
                    Some(r) if r.starts_with(NO_COMMAND_LOOKED_FOR) => format!(
                        "No test command was found, so no clean-checkout test report was made: {r}."
                    ),
                    _ => "No test command was found, so no clean-checkout test report was made."
                        .into(),
                };
            }
            TestReportStatus::NotRun => {
                return match self.why() {
                    Some(r) => format!("The clean-checkout test report was not made: {r}."),
                    None => "The clean-checkout test report was not made.".into(),
                };
            }
            TestReportStatus::Ran | TestReportStatus::TimedOut => {}
        }
        if let Some(claim) = self.claim() {
            let skipped = self.counts.map_or(0, |c| c.skipped);
            let claim = if skipped > 0 {
                format!("{claim} ({skipped} skipped).")
            } else {
                format!("{claim}.")
            };
            // The claim is about the repository's tests with the services the report supplied: it says so beside the claim.
            return if self.services_started.is_empty() {
                claim
            } else {
                let list = self.services_started.join(" and ");
                format!(
                    "{claim} The report started a local {list} server in place of the Compose service the repository declares."
                )
            };
        }
        // Commands that did not run and nothing that failed: not red, and not a claim either.
        if self.status == TestReportStatus::Ran
            && self.failed_command.is_none()
            && let Some(r) = self.why().filter(|r| r.starts_with(NOT_RUN_PREFIX))
        {
            return format!("{r}.");
        }
        if self.status == TestReportStatus::TimedOut {
            return match &self.timed_out {
                // The output shows build work and no test that began: the sentence says so and claims nothing about the tests.
                Some(t) if self.stopped_while_building() => format!(
                    "`{}` was stopped at its limit of {} s while it was still building: its output shows build work and no test that began, so this report says nothing about the tests.",
                    quoted(&t.command),
                    t.after_s
                ),
                // Build work in the output of a runner whose tests may begin without a line the reading knows: the sentence says what the
                // output shows and does not say that no test began.
                Some(t) if self.stopped_in_build_work() => format!(
                    "`{}` was stopped at its limit of {} s; the end of its output shows build work, and the report cannot tell whether a test began, so it says nothing about the tests.",
                    quoted(&t.command),
                    t.after_s
                ),
                Some(t) => format!(
                    "`{}` timed out after {} s; the commands that finished are listed in the report.",
                    quoted(&t.command),
                    t.after_s
                ),
                None => "The report's time ran out before every command ran; the commands that finished are listed in the report.".to_string(),
            };
        }
        let failed = quoted(
            self.failed_command
                .as_deref()
                .or(self.command.as_deref())
                .unwrap_or("the test command"),
        );
        let code = self
            .exit_code
            .map_or_else(|| "no code".to_string(), |c| c.to_string());
        if self.setup_failed {
            return format!(
                "Setup `{failed}` exited {code} in a clean checkout, so the test command did not run."
            );
        }
        // A step that exited non-zero while a service it needs was not available keeps its exit code and counts; the sentence says what it ran without,
        // and does not say the tests failed for that reason.
        if !self.needs.is_empty() {
            let list = self.needs.join(" and ");
            return match self.counts {
                None => format!(
                    "`{failed}` exited {code} in a clean checkout while {list} was not available; the runner's test counts could not be read."
                ),
                Some(c) => format!(
                    "`{failed}` exited {code} in a clean checkout while {list} was not available; the runner reported {} passed, {} failed.",
                    c.passed, c.failed
                ),
            };
        }
        match self.counts {
            None => format!(
                "`{failed}` exited {code} in a clean checkout; the runner's test counts could not be read."
            ),
            Some(c) if self.all_exit_zero && c.failed == 0 && c.passed == 0 => {
                format!("`{failed}` exited 0 in a clean checkout and the runner reported 0 tests.")
            }
            Some(c) => format!(
                "`{failed}` exited {code} in a clean checkout; the runner reported {} passed, {} failed.",
                c.passed, c.failed
            ),
        }
    }
}

// ---- what the report is doing ----------------------------------------------------------------------------

/// The `kind` of the `turn.progress` events agentd writes while the clean-checkout test report runs. They are ordinary progress events of the
/// direct agent (agent authority): they change no status, no phase and no outcome, and a client that does not know the kind shows their `text`
/// as the agent's newest words.
pub const PROGRESS_REPORT: &str = "report";
/// The most characters of `command` a progress payload keeps.
pub const MAX_PROGRESS_COMMAND_CHARS: usize = 120;
/// The most characters of `text` a progress payload keeps.
pub const MAX_PROGRESS_TEXT_CHARS: usize = 200;

/// Where the report stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportProgressState {
    /// The report began: the clean checkout is made and the commands are planned.
    Checkout,
    /// A command starts now (`step` of `of`, with `command` and `limitS`).
    Running,
    /// The commands are over (`status` says how); the upload follows.
    Ended,
}

/// The payload of a `turn.progress` event of kind [`PROGRESS_REPORT`]. A reader ignores a payload whose kind or state it does not know
/// ([`ReportProgress::from_value`]) and a member it does not know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportProgress {
    /// Always [`PROGRESS_REPORT`].
    pub kind: String,
    pub state: ReportProgressState,
    /// One plain line for the live row of the screen, at most [`MAX_PROGRESS_TEXT_CHARS`] characters.
    #[serde(default)]
    pub text: String,
    /// `running` only: the 1-based number of the command and how many are planned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<u32>,
    /// `running` only: the command, secrets redacted by the writer, at most [`MAX_PROGRESS_COMMAND_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `running` only: the time limit this command was given, in whole seconds ([`TestStep::limit_s`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_s: Option<u32>,
    /// `ended` only: how the report ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TestReportStatus>,
}

impl ReportProgress {
    fn base(state: ReportProgressState, text: &str) -> Self {
        Self {
            kind: PROGRESS_REPORT.to_string(),
            state,
            text: cut_chars(&plain(text), MAX_PROGRESS_TEXT_CHARS),
            step: None,
            of: None,
            command: None,
            limit_s: None,
            status: None,
        }
    }

    /// The report began.
    pub fn checkout(text: &str) -> Self {
        Self::base(ReportProgressState::Checkout, text)
    }

    /// Command `step` of `of` starts, with `limit_s` seconds.
    pub fn running(step: u32, of: u32, command: &str, limit_s: u32, text: &str) -> Self {
        Self {
            step: Some(step),
            of: Some(of),
            command: Some(cut_chars(&plain(command), MAX_PROGRESS_COMMAND_CHARS)),
            limit_s: Some(limit_s),
            ..Self::base(ReportProgressState::Running, text)
        }
    }

    /// The commands are over.
    pub fn ended(status: TestReportStatus, text: &str) -> Self {
        Self {
            status: Some(status),
            ..Self::base(ReportProgressState::Ended, text)
        }
    }

    /// The event payload.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The payload an event carries, or `None` when its kind is not [`PROGRESS_REPORT`] or its state is one this build does not know.
    pub fn from_value(v: &Value) -> Option<Self> {
        serde_json::from_value::<Self>(v.clone())
            .ok()
            .filter(|p| p.kind == PROGRESS_REPORT)
    }
}
