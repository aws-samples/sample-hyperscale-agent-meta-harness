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
/// How the reason of a step starts when agentd ran it and the runner refused the command as the lifecycle log wrote it: a word of the
/// command names no file or directory of the clean checkout, which agentd checked itself, so a repository's output alone cannot make such a step.
/// The step is no failure of the repository's tests, it keeps the claim away, and the sentence names it after the commands that ran.
pub const NOT_AS_WRITTEN: &str = "could not run as written";
/// How the reason of a step starts when the install step it needs failed and it was not started: `skipped: the setup step `npm ci` failed`.
pub const SKIPPED_FOR_SETUP: &str = "skipped: the setup step ";
/// The most characters of a setup command that the reason of a step it stopped carries.
pub const MAX_SETUP_REASON_COMMAND_CHARS: usize = 120;
/// The most characters of such a reason that the summary keeps: the whole of it, so the setup it names can be read back.
const MAX_SETUP_REASON_CHARS: usize = 170;

/// The reason of a step that was not started because the setup step `command` did not give it what it needs: `skipped:
/// the setup step `npm ci` failed`, or `... could not be read` for a setup whose command was not run at all (`failed` is false). The
/// command is cut to [`MAX_SETUP_REASON_COMMAND_CHARS`] characters, with a backtick of its own as an apostrophe.
pub fn skipped_for_setup_reason(command: &str, failed: bool) -> String {
    let how = if failed {
        "failed"
    } else {
        "could not be read"
    };
    format!("{SKIPPED_FOR_SETUP}`{}` {how}", setup_in_reason(command))
}

/// A setup command as the reason of a step it stopped carries it.
fn setup_in_reason(command: &str) -> String {
    command
        .chars()
        .take(MAX_SETUP_REASON_COMMAND_CHARS)
        .map(|c| if c == '`' { '\'' } else { c })
        .collect()
}

/// The setup command that a reason [`skipped_for_setup_reason`] wrote names. `None` for any other reason, and for one that was cut.
fn setup_named(reason: &str) -> Option<&str> {
    let rest = reason.strip_prefix(SKIPPED_FOR_SETUP)?.strip_prefix('`')?;
    let (command, how) = rest.rsplit_once('`')?;
    matches!(how, " failed" | " could not be read").then_some(command)
}

/// How the reason of a step starts when the report stopped it at its time limit and lists it as not run: `stopped at its time
/// limit of 300 s`. Only a step the report added behind the model's own commands, in place of logged commands that name a part of a
/// suite, gets it. The commands of the model have run by then, so their result stands, the report stays `ran`, and the sentence names
/// the stopped command behind them.
pub const STOPPED_CHOICE: &str = "stopped at its time limit";
/// The reason of such a step that was never started because the report's time had ended.
pub const NO_TIME_FOR_CHOICE: &str = "the report's time ended before it started";
/// The start of the head line agentd writes on a test step it chose from the layout of the tree to run in place of test commands of the
/// lifecycle log that it left out: ``PLAN NOTE: in place of `<command>` ``, with `` and N more`` for more such commands.
/// [`in_place_line`] is the whole line and [`TestStep::in_place_of`] reads it. It starts like the plan note ([`PLAN_NOTE_PREFIX`]), so a
/// reader that predates it passes over it.
pub const IN_PLACE_PREFIX: &str = "PLAN NOTE: in place of ";
/// The start of the head lines agentd writes on the first step of a report whose plan left test commands of the lifecycle log out with
/// nothing in their place: ``PLAN NOTE: left out `<command>` ``, with `` and N more`` and, after `; `, the reason when it is not
/// that the report found no command for the whole suite ([`LeftOutWhy`]). One line for each reason. [`left_out_line_of`] is the whole line
/// and [`TestStep::left_out_notes`] reads the lines.
pub const LEFT_OUT_PREFIX: &str = "PLAN NOTE: left out ";
/// The most characters of the file name that a line of [`LEFT_OUT_PREFIX`] carries.
pub const MAX_PLAN_LINE_FILE_CHARS: usize = 60;
/// The most characters of the command that a line of [`IN_PLACE_PREFIX`] or [`LEFT_OUT_PREFIX`] quotes.
pub const MAX_PLAN_LINE_COMMAND_CHARS: usize = 80;

/// The line agentd writes for a step that runs in place of `command` and `more` other commands of the lifecycle log.
pub fn in_place_line(command: &str, more: usize) -> String {
    plan_line(IN_PLACE_PREFIX, command, more)
}

/// The line agentd writes for `command` and `more` other commands of the lifecycle log that the plan left out with nothing in their place
/// because the report found no command for the whole suite.
pub fn left_out_line(command: &str, more: usize) -> String {
    left_out_line_of(command, more, &LeftOutWhy::NoSuiteCommand, None)
}

/// Why the plan left test commands of the lifecycle log out with nothing in their place. Each such command names a part of a suite,
/// so it is no command of the plan; this says why no command of the report's own choosing stands in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeftOutWhy {
    /// The report found no command for the whole suite: the tree has no layout it knows, or the command works in another folder.
    NoSuiteCommand,
    /// A file of the tree names packages that the report does not install, so the suite would fail for want of a step that is the report's
    /// own. The line carries the name of the file when it is plain ([`plain_file_name`]).
    Packages(Option<String>),
    /// The report has a command for the whole suite of the language, and that command does not run the file the logged command names: a
    /// unittest discovery starts in other folders, or pytest does not collect the file.
    NotReached,
    /// The logged commands of the language work in more than one project folder, so one command at the root is not what the log ran.
    Projects,
}

/// What a line of [`LEFT_OUT_PREFIX`] says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOutNote<'a> {
    /// The last of the commands that was run.
    pub command: &'a str,
    /// The number of the other commands.
    pub more: usize,
    pub why: LeftOutWhy,
    /// What the lifecycle log said of the last run of `command`, when that run did not pass: the exit code and the number of failed
    /// tests, as far as the log gave them. The command was not run in the clean checkout, so this is the log's word alone.
    pub logged: Option<LoggedRun>,
}

/// Whether `name` is a file name a line about the plan may carry: at most [`MAX_PLAN_LINE_FILE_CHARS`] characters, each a letter, a digit
/// or one of `._-/`. The name of a file is the repository's own text, and the line is read back word for word.
pub fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_PLAN_LINE_FILE_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

/// The line agentd writes for `command` and `more` other commands of the lifecycle log that the plan left out with nothing in their place,
/// with the reason: [`left_out_line`] for [`LeftOutWhy::NoSuiteCommand`], ``...; packages in <file>`` (or ``...; packages`` for a
/// file whose name is not plain) for [`LeftOutWhy::Packages`], ``...; not reached`` for [`LeftOutWhy::NotReached`] and
/// ``...; more than one project`` for [`LeftOutWhy::Projects`]. `logged` is what the lifecycle log said of the last run of `command`: when
/// that run did not pass, the line ends ``; logged exit <code>, <n> failed`` ([`logged_words`]), so the detail line can say that the
/// command the report left out was one the log recorded as failed.
pub fn left_out_line_of(
    command: &str,
    more: usize,
    why: &LeftOutWhy,
    logged: Option<&LoggedRun>,
) -> String {
    let mut line = plan_line(LEFT_OUT_PREFIX, command, more);
    match why {
        LeftOutWhy::NoSuiteCommand => {}
        LeftOutWhy::Packages(Some(file)) if plain_file_name(file) => {
            line.push_str(&format!("; packages in {file}"));
        }
        LeftOutWhy::Packages(_) => line.push_str("; packages"),
        LeftOutWhy::NotReached => line.push_str("; not reached"),
        LeftOutWhy::Projects => line.push_str("; more than one project"),
    }
    if let Some(words) = logged.and_then(logged_words) {
        line.push_str(&format!("; logged {words}"));
    }
    line
}

/// What the lifecycle log said of a run that did not pass, in the words a line about the plan and its detail line carry: `exit <code>,
/// <n> failed`; `exit <code>` for a code other than 0 with no failed tests counted; `<n> failed` when the log gave no exit code as a
/// number. `None` for a run that passed, and for one the log said nothing of.
fn logged_words(run: &LoggedRun) -> Option<String> {
    let failed = run.failed.filter(|n| *n > 0);
    match (run.exit_code, failed) {
        (Some(code), Some(failed)) => Some(format!("exit {code}, {failed} failed")),
        (Some(code), None) if code != 0 => Some(format!("exit {code}")),
        (None, Some(failed)) => Some(format!("{failed} failed")),
        _ => None,
    }
}

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

    /// The lines of the output from the place where agentd writes its lines about the plan: after the `NOT RUN:` line of a step it
    /// marked as not run and after the note of a Cargo workspace ([`TestStep::plan_note_members`]).
    fn plan_lines(&self) -> impl Iterator<Item = &str> {
        let mut lines = self.output_tail.lines().peekable();
        if self.is_not_run() && lines.peek().is_some_and(|l| l.starts_with(NOT_RUN_PREFIX)) {
            lines.next();
        }
        if self.plan_note_members().is_some() {
            lines.next();
        }
        lines
    }

    /// The commands of the lifecycle log this step ran in place of, as the last one run and the number of the others: the line
    /// [`in_place_line`] at the head of the output of a test step the harness chose from the layout of the tree (source `Detected`). agentd writes
    /// the line at that place and removes a line of that form that the repository printed there, and no other step and no other place is read,
    /// so the output of a command cannot say what a step stands for. A line that is not in the form agentd writes gives `None`.
    pub fn in_place_of(&self) -> Option<(&str, usize)> {
        if self.role != TestStepRole::Test || self.source != TestCommandSource::Detected {
            return None;
        }
        plan_line_parts(self.plan_lines().next()?, IN_PLACE_PREFIX)
    }

    /// The commands of the lifecycle log the plan left out with nothing in their place, one note for each reason: the lines
    /// [`left_out_line_of`] at the head of the output, after the line of [`TestStep::in_place_of`] when the step has one. agentd writes them
    /// on the first step of a report and removes a line of that form that the repository printed at that place of any step. The reading
    /// ends at the first line that is not wholly in the form agentd writes. A service step, whose output is the report's own helper's,
    /// has none.
    pub fn left_out_notes(&self) -> Vec<LeftOutNote<'_>> {
        if self.command.starts_with(SERVICE_COMMAND_PREFIX) {
            return Vec::new();
        }
        let mut lines = self.plan_lines().peekable();
        if lines.peek().is_some_and(|l| l.starts_with(IN_PLACE_PREFIX)) {
            lines.next();
        }
        lines.map_while(left_out_parts).collect()
    }

    /// The first of [`TestStep::left_out_notes`], as the last command run and the number of the others.
    pub fn left_out(&self) -> Option<(&str, usize)> {
        self.left_out_notes().first().map(|n| (n.command, n.more))
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
    /// did not run shows nothing. The summary's `reason` then names the first such command and why (`NOT RUN:...`) when the report has none;
    /// a command that could not run as written is named only when no command was left out for another reason.
    ///
    /// `setup_failed` holds when the first step that did not exit 0 is a setup step: the tests that need it did not run, and a test that
    /// failed before it leads the sentence instead.
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
            // A command that could not run as written says least about the repository: it is named only when nothing else was left out.
            let (written, others): (Vec<&TestStep>, Vec<&TestStep>) =
                not_run.iter().copied().partition(|s| {
                    s.not_run_reason()
                        .is_some_and(|r| r.starts_with(NOT_AS_WRITTEN))
                });
            let group = if others.is_empty() { written } else { others };
            fn why_of(s: &TestStep) -> &str {
                s.not_run_reason().unwrap_or("a service is missing")
            }
            // The setup that failed first leads the sentence, so the commands it stopped are named first when the report lists any.
            let stopped_by_first = first_failing
                .filter(|s| s.role == TestStepRole::Setup)
                .and_then(|setup| {
                    let text = setup_in_reason(&setup.command);
                    group
                        .iter()
                        .copied()
                        .find(|s| setup_named(why_of(s)) == Some(text.as_str()))
                });
            let first = stopped_by_first.or_else(|| group.first().copied())?;
            // Commands a setup stopped are counted with that setup alone, and a second setup that stopped commands of its own is named
            // behind them with its own count. Any other reason counts every command that was left out, as before.
            let by_setup = setup_named(why_of(first)).is_some();
            let more_of_the_reason =
                |s: &TestStep| match group.iter().filter(|o| why_of(o) == why_of(s)).count() {
                    0 | 1 => String::new(),
                    n => format!(", and {} more", n - 1),
                };
            let named_with_reason = |s: &TestStep, chars: usize| {
                format!(
                    "`{}` ({})",
                    quoted(&cut_chars(&s.command, MAX_REPORT_COMMAND_CHARS)),
                    cut_chars(why_of(s), chars)
                )
            };
            if !by_setup {
                let more = match group.len() {
                    1 => String::new(),
                    n => format!(", and {} more", n - 1),
                };
                return Some(format!(
                    "{NOT_RUN_PREFIX}{}{more}",
                    named_with_reason(first, 120)
                ));
            }
            let also = group
                .iter()
                .copied()
                .find(|s| why_of(s) != why_of(first) && setup_named(why_of(s)).is_some())
                .map(|s| {
                    format!(
                        "{ALSO_STOPPED}{}{}",
                        named_with_reason(s, MAX_SETUP_REASON_CHARS),
                        more_of_the_reason(s)
                    )
                })
                .unwrap_or_default();
            Some(format!(
                "{NOT_RUN_PREFIX}{}{}{also}",
                named_with_reason(first, MAX_SETUP_REASON_CHARS),
                more_of_the_reason(first)
            ))
        });
        // The plan note is the second line of the reason: the first line stays the report's own reason or the not-run reason, so a note never
        // hides either.
        let note = self.steps.iter().find_map(|s| {
            s.plan_note_members()
                .map(|members| plan_note_text(members, s.in_place_of().is_some()))
        });
        let reason = match note {
            Some(note) => Some(format!("{}\n{note}", reason.unwrap_or_default())),
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
            setup_failed: first_failing.is_some_and(|s| s.role == TestStepRole::Setup),
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
    /// ran without. Setup steps other than service steps have no line. A test step the harness chose from the layout of the tree in
    /// place of commands of the lifecycle log says which ([`TestStep::in_place_of`]), and the commands of the log that the plan left out with
    /// nothing in their place have a line for each reason behind the step that carries them, the first of the report
    /// ([`TestStep::left_out_notes`]), so the lines are among those kept.
    ///
    /// The details hold at most [`MAX_SUMMARY_DETAILS`] lines. A line about the plan (the line of a step that stands in place of logged
    /// commands, a line of commands left out) is kept whatever its place, because [`TestReportSummary::sentence`] reads those lines; the
    /// other lines fill the rest in their order.
    fn step_details(&self) -> Vec<String> {
        // Each line with whether it is about the plan.
        let mut lines: Vec<(String, bool)> = Vec::new();
        for s in &self.steps {
            let left_out: Vec<String> = s.left_out_notes().iter().map(left_out_detail).collect();
            let line = if let Some(sentence) = s.stand_in() {
                Some(match (s.is_not_run(), s.exit_code) {
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
                })
            } else if s.role == TestStepRole::Test {
                // What the line says of the logged commands the step stands in place of. A step that was not run, or was stopped
                // at its limit, did not run in their place: it stands there.
                let in_place_of = s.in_place_of().map(|(command, more)| {
                    let words = if s.is_not_run() || s.timed_out {
                        STANDS_IN_PLACE_OF
                    } else {
                        RAN_IN_PLACE_OF
                    };
                    format!(
                        "{words}{}{OF_THE_LOG}, which {}",
                        named(command, more),
                        names_a_part(more)
                    )
                });
                // Those words close the line, and the sentence of the card reads them: the reason of a step that was not run gives way
                // to them, so the line ends whole inside its limit.
                let reason_room = in_place_of.as_ref().map_or(120, |words| {
                    let fixed = quoted(&s.command).chars().count()
                        + "``: not run ().".len()
                        + words.chars().count();
                    MAX_SUMMARY_DETAIL_CHARS
                        .saturating_sub(fixed)
                        .clamp(24, 120)
                });
                let result = if s.is_not_run() {
                    format!(
                        "not run ({})",
                        cut_chars(s.not_run_reason().unwrap_or("it was not run"), reason_room)
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
                let mut line = format!("`{}`: {result}", quoted(&s.command));
                let ran_without = s.needs();
                if !ran_without.is_empty() {
                    let list: Vec<String> = ran_without
                        .iter()
                        .map(|(n, why)| format!("{n} ({})", cut_chars(why, 100)))
                        .collect();
                    line.push_str(&format!("; it ran without {}", list.join(", ")));
                }
                if let Some(words) = in_place_of {
                    // A line that is still too long (a long command of the step's own) gives way from its end, in front of the words.
                    let room = MAX_SUMMARY_DETAIL_CHARS.saturating_sub(words.chars().count() + 1);
                    line = cut_chars(&line, room);
                    line.push_str(&words);
                }
                line.push('.');
                Some(line)
            } else {
                None
            };
            let in_place = s.role == TestStepRole::Test && s.in_place_of().is_some();
            lines.extend(line.map(|l| (l, in_place)));
            lines.extend(left_out.into_iter().map(|l| (l, true)));
        }
        let about_plan = lines.iter().filter(|(_, plan)| *plan).count();
        let mut plan_room = MAX_SUMMARY_DETAILS;
        let mut room = MAX_SUMMARY_DETAILS.saturating_sub(about_plan);
        lines
            .into_iter()
            .filter(|(_, plan)| {
                let left = if *plan { &mut plan_room } else { &mut room };
                let keep = *left > 0;
                *left = left.saturating_sub(1);
                keep
            })
            .map(|(line, _)| cut_chars(&plain(&line), MAX_SUMMARY_DETAIL_CHARS))
            .collect()
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
    /// marked `outputTruncated`. The end of every output is the part kept. A cut takes agentd's head lines with the front, so a line of the
    /// forms [`IN_PLACE_PREFIX`] and [`LEFT_OUT_PREFIX`] that then leads the output is the repository's and is removed.
    /// `validate` passes on the result for the same run and head.
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
            s.output_tail = if cut { without_plan_lines(tail) } else { tail };
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
            self.steps[i].output_tail =
                without_plan_lines(tail_bytes(&self.steps[i].output_tail, half).0);
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

/// `tail`, the end of an output whose front was cut off, without the lines of the forms [`IN_PLACE_PREFIX`] and [`LEFT_OUT_PREFIX`] at
/// its head. agentd writes those lines at the head of the whole output, so a line of that form that leads what a cut left is one
/// the repository printed, and nothing may read it as agentd's.
fn without_plan_lines(tail: String) -> String {
    let mut rest = tail.as_str();
    while rest.starts_with(IN_PLACE_PREFIX) || rest.starts_with(LEFT_OUT_PREFIX) {
        rest = rest.split_once('\n').map_or("", |(_, after)| after);
    }
    if rest.len() == tail.len() {
        tail
    } else {
        rest.to_string()
    }
}

/// The note for a `cargo test` the harness chose from the manifest of a workspace of `members` members. It opens with why the
/// harness chose the command: the repository names no test command, or, for a step that stands in place of logged commands,
/// the lifecycle log names only a part of the Rust tests; such a step may stand beside commands the log does name.
fn plan_note_text(members: u32, in_place: bool) -> String {
    let why = if in_place {
        "The lifecycle log names only a part of the Rust tests"
    } else {
        "The repository names no test command"
    };
    format!(
        "{why}, so the harness chose `{PLAN_NOTE_COMMAND}` from the manifest (Cargo.toml). It runs the tests of the whole workspace, {members} members. A `.ham/test-command` file in the repository names a narrower command."
    )
}

/// A command as it stands inside backticks in a sentence: a backtick of its own becomes an apostrophe.
fn quoted(command: &str) -> String {
    plain(command).replace('`', "'")
}

/// The words between the commands the first failed setup stopped and the commands a second one stopped, in the line
/// [`TestReport::summary`] writes in `reason`.
const ALSO_STOPPED: &str = "; also ";

/// The parts of the line [`TestReport::summary`] writes in `reason` for the first command that was not run, ``NOT RUN: `<command>` (<why>)``
/// with `, and N more` for more such commands: the command, the why and N. `None` for any other line. What the line says of a second
/// setup ([`also_stopped`]) is no part of them.
fn not_run_parts(line: &str) -> Option<(&str, &str, usize)> {
    let line = match also_stopped(line) {
        Some(_) => line
            .rsplit_once(ALSO_STOPPED)
            .map_or(line, |(head, _)| head),
        None => line,
    };
    group_parts(line.strip_prefix(NOT_RUN_PREFIX)?)
}

/// The parts of one group of commands that were not run, `` `<command>` (<why>)`` with `, and N more`: the command, the why and N.
fn group_parts(group: &str) -> Option<(&str, &str, usize)> {
    let rest = group.strip_prefix('`')?;
    let (command, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(" (")?;
    // The count is the end of the group. Words of a command inside the why that look like its start (`), and `) are no count.
    let counted = rest.rsplit_once("), and ").and_then(|(why, n)| {
        let more = n.strip_suffix(" more")?.parse::<usize>().ok()?;
        Some((why, more))
    });
    let (why, more) = match counted {
        Some(parts) => parts,
        None => (rest.strip_suffix(')')?, 0),
    };
    Some((command, why, more))
}

/// What the line [`TestReport::summary`] writes in `reason` says of a second setup that failed and stopped commands of its own:
/// the first of those commands, the setup and the number of the other commands. The words are read only in the form the summary writes,
/// ``; also `<command>` (skipped: the setup step `<setup>` failed)`` at the end of the line. `None` for a line without them.
fn also_stopped(line: &str) -> Option<(&str, &str, usize)> {
    let (_, group) = line.rsplit_once(ALSO_STOPPED)?;
    let (command, why, more) = group_parts(group)?;
    Some((command, setup_named(why)?, more))
}

/// `` `<command>` `` or `` `<command>` and N more ``.
fn named(command: &str, more: usize) -> String {
    match more {
        0 => format!("`{command}`"),
        n => format!("`{command}` and {n} more"),
    }
}

/// A head line about the plan: the prefix, then the command in backticks, cut to [`MAX_PLAN_LINE_COMMAND_CHARS`] characters, and the
/// number of the other commands.
fn plan_line(prefix: &str, command: &str, more: usize) -> String {
    let command = cut_chars(&quoted(command), MAX_PLAN_LINE_COMMAND_CHARS);
    format!("{prefix}{}", named(&command, more))
}

/// The command and the number of the other commands of a line [`plan_line`] wrote with `prefix`. The whole line must be in that form: a
/// command in backticks that is not empty, then nothing or `` and N more`` with an N of at least 1. `None` for any other line, so words written
/// after the command are never read.
fn plan_line_parts<'a>(line: &'a str, prefix: &str) -> Option<(&'a str, usize)> {
    let rest = line.strip_prefix(prefix)?.strip_prefix('`')?;
    let (command, rest) = rest.split_once('`')?;
    if command.is_empty() {
        return None;
    }
    Some((command, more_of(rest)?))
}

/// The largest number of other commands a plan line may count. A lifecycle log is read up to 1 MiB, so it holds far fewer rows; a larger
/// number is not agentd's, and the counts of the lines can be added without care for overflow.
const MAX_PLAN_LINE_MORE: usize = 1_000_000;

/// The number a plan line gives after its command: 0 for nothing, N for `` and N more`` with an N of 1 to [`MAX_PLAN_LINE_MORE`]. `None`
/// for anything else.
fn more_of(counted: &str) -> Option<usize> {
    if counted.is_empty() {
        return Some(0);
    }
    let digits = counted.strip_prefix(" and ")?.strip_suffix(" more")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=MAX_PLAN_LINE_MORE).contains(n))
}

/// The reason a line of [`LEFT_OUT_PREFIX`] gives after `; `, in the words [`left_out_line_of`] writes. `None` for any other words.
fn left_out_why(field: &str) -> Option<LeftOutWhy> {
    match field {
        "packages" => return Some(LeftOutWhy::Packages(None)),
        "not reached" => return Some(LeftOutWhy::NotReached),
        "more than one project" => return Some(LeftOutWhy::Projects),
        _ => {}
    }
    let file = field.strip_prefix("packages in ")?;
    plain_file_name(file).then(|| LeftOutWhy::Packages(Some(file.to_string())))
}

/// `text` as a number written the way a number is printed: digits, with `-` in front of a negative one, and no leading zero.
fn printed_number<T: std::str::FromStr + ToString>(text: &str) -> Option<T> {
    text.parse::<T>().ok().filter(|n| n.to_string() == text)
}

/// What a line of [`LEFT_OUT_PREFIX`] says of the last logged run of its command behind `; `: the words `logged` and [`logged_words`].
/// `None` for any other words, also for words of a run that passed, which agentd does not write.
fn logged_failure(field: &str) -> Option<LoggedRun> {
    let words = field.strip_prefix("logged ")?;
    let (code, failed) = match words.strip_prefix("exit ") {
        None => (None, Some(words)),
        Some(rest) => match rest.split_once(", ") {
            Some((code, failed)) => (Some(code), Some(failed)),
            None => (Some(rest), None),
        },
    };
    let exit_code = match code {
        Some(text) => Some(printed_number::<i32>(text)?),
        None => None,
    };
    let failed = match failed {
        Some(text) => Some(printed_number::<u32>(text.strip_suffix(" failed")?)?),
        None => None,
    };
    let run = LoggedRun {
        exit_code,
        passed: None,
        failed,
    };
    (logged_words(&run).as_deref() == Some(words)).then_some(run)
}

/// What a line [`left_out_line_of`] wrote says. The whole line must be in that form: the command in backticks, the number of the others,
/// then, each behind `; ` and in agentd's own words, the reason when there is one and what the log recorded when the last run did not
/// pass. `None` for any other line, so words a repository wrote after a line of this form are never read.
fn left_out_parts(line: &str) -> Option<LeftOutNote<'_>> {
    let rest = line.strip_prefix(LEFT_OUT_PREFIX)?.strip_prefix('`')?;
    let (command, rest) = rest.split_once('`')?;
    if command.is_empty() {
        return None;
    }
    let mut fields = rest.split("; ");
    let more = more_of(fields.next()?)?;
    let mut why = LeftOutWhy::NoSuiteCommand;
    let mut field = fields.next();
    if let Some(known) = field.and_then(left_out_why) {
        why = known;
        field = fields.next();
    }
    let mut logged = None;
    if let Some(words) = field {
        logged = Some(logged_failure(words)?);
        field = fields.next();
    }
    if field.is_some() {
        return None;
    }
    Some(LeftOutNote {
        command,
        more,
        why,
        logged,
    })
}

/// The words of a detail line between the result of a step and the commands of the lifecycle log it ran in place of.
const RAN_IN_PLACE_OF: &str = "; it ran in place of ";
/// The same for a step that was not run or was stopped at its limit: it did not run in place of the commands, it stands there.
const STANDS_IN_PLACE_OF: &str = "; it stands in place of ";
/// The words of a detail line behind the commands of the lifecycle log it names.
const OF_THE_LOG: &str = " of the lifecycle log";

/// The number of commands of the lifecycle log that a detail line says its step stands in place of: the words
/// [`TestReport::summary`] writes behind the result of such a step. `None` for a line without them. A command inside backticks holds no
/// backtick of its own ([`quoted`]), so the words of a command are never taken for them.
fn in_place_count(line: &str) -> Option<usize> {
    [RAN_IN_PLACE_OF, STANDS_IN_PLACE_OF]
        .iter()
        .flat_map(|words| line.match_indices(words))
        .find_map(|(at, words)| {
            let rest = line[at + words.len()..].strip_prefix('`')?;
            let (_, rest) = rest.split_once('`')?;
            let (counted, rest) = rest.split_once(OF_THE_LOG)?;
            rest.starts_with(", which name")
                .then(|| more_of(counted))
                .flatten()
                .map(|more| more + 1)
        })
}

/// The number of commands of the lifecycle log that a detail line says were left out with nothing in their place: a line
/// [`left_out_detail`] wrote. `None` for any other line.
fn left_out_count(line: &str) -> Option<usize> {
    let rest = line.strip_prefix('`')?;
    let (_, rest) = rest.split_once('`')?;
    let (counted, rest) = rest.split_once(OF_THE_LOG)?;
    (rest.starts_with(" was left out: ") || rest.starts_with(" were left out: "))
        .then(|| more_of(counted))
        .flatten()
        .map(|more| more + 1)
}

/// The detail line of commands of the lifecycle log that the plan left out with nothing in their place: which, that each names a
/// part of a suite, why no command of the report stands in their place, and what the log recorded for the command named when its last
/// run did not pass.
fn left_out_detail(note: &LeftOutNote<'_>) -> String {
    let (verb, they) = if note.more == 0 {
        ("was", "it")
    } else {
        ("were", "they")
    };
    let why = match &note.why {
        LeftOutWhy::NoSuiteCommand => "the report found no command for the whole suite".to_string(),
        LeftOutWhy::Packages(Some(file)) => {
            format!("{file} names packages that the report does not install")
        }
        LeftOutWhy::Packages(None) => {
            "a file of the repository names packages that the report does not install".to_string()
        }
        LeftOutWhy::NotReached if note.more == 0 => {
            "the report found no command for the whole suite that runs the file it names"
                .to_string()
        }
        LeftOutWhy::NotReached => {
            "the report found no command for the whole suite that runs the files they name"
                .to_string()
        }
        LeftOutWhy::Projects => {
            "the log runs these tests in more than one project folder, so the report added no command at the root"
                .to_string()
        }
    };
    let recorded = match note.logged.as_ref().and_then(logged_words) {
        None => String::new(),
        Some(words) if note.more == 0 => format!("; the log recorded {words} for it"),
        Some(words) => format!("; the log recorded {words} for `{}`", note.command),
    };
    format!(
        "{}{OF_THE_LOG} {verb} left out: {they} {}, and {why}{recorded}.",
        named(note.command, note.more),
        names_a_part(note.more)
    )
}

/// What the commands of a plan line have in common, as the words after `which` or `it`: each names a part of a suite.
fn names_a_part(more: usize) -> &'static str {
    match more {
        0 => "names a test file, a test or a filter",
        _ => "name test files, tests or filters",
    }
}

impl TestReportSummary {
    /// What the runner reported for the test commands that ran, as the end of a sentence.
    fn counts_words(&self) -> String {
        match self.counts {
            Some(c) => format!(
                "the runner reported {} passed, {} failed",
                c.passed, c.failed
            ),
            None => "the runner's test counts could not be read".to_string(),
        }
    }

    /// The sentence of a report whose only commands left out could not run as written: the commands that ran come first, then the
    /// command the log wrote with the words the runner refused. `None` when `line` is about another reason.
    fn as_written_sentence(&self, line: &str) -> Option<String> {
        let (command, why, more) = not_run_parts(line)?;
        let rest = why.strip_prefix(NOT_AS_WRITTEN)?;
        // The reason is the first command's, and it names that command's path. For `<command> and N more` it would give that path to the others,
        // so the sentence says what each of them lacks without naming it.
        let rest = if more == 0 {
            rest
        } else {
            ": the clean checkout has no file or directory that each of them names"
        };
        let written = format!("{} {NOT_AS_WRITTEN}{rest}.", named(command, more));
        Some(match self.command {
            None => written,
            Some(_) => format!(
                "The test commands that ran exited 0 in a clean checkout; {}. {written}",
                self.counts_words()
            ),
        })
    }

    /// The sentence of a report whose first command left out is one the report added behind the model's own commands and then stopped at
    /// its time limit ([`STOPPED_CHOICE`]) or never started for want of time ([`NO_TIME_FOR_CHOICE`]): the commands that ran come
    /// first, as for a command that could not run as written, then the command and what happened to it. `None` when `line` is about
    /// another reason.
    fn choice_not_run_sentence(&self, line: &str) -> Option<String> {
        let (command, why, more) = not_run_parts(line)?;
        let what = if let Some(limit) = why.strip_prefix(STOPPED_CHOICE) {
            format!("`{command}` was stopped at its time limit{limit}")
        } else if why == NO_TIME_FOR_CHOICE {
            format!("`{command}` did not run: {NO_TIME_FOR_CHOICE}")
        } else {
            return None;
        };
        let others = match more {
            0 => String::new(),
            1 => ", and 1 more command did not run".to_string(),
            n => format!(", and {n} more commands did not run"),
        };
        let chosen = format!("{what}{others}.");
        Some(match self.command {
            None => chosen,
            Some(_) => format!(
                "The test commands that ran exited 0 in a clean checkout; {}. {chosen}",
                self.counts_words()
            ),
        })
    }

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

    /// The words for EVIDENCE.md, the pull request and `ham pull`: one plain sentence about the commands of the report, the claim when it
    /// is allowed, else what happened ([`TestReportSummary::steps_sentence`]). For a report whose plan chose a command in place of test
    /// commands of the lifecycle log, or left some out, a second sentence says so and where the details are.
    /// The claim keeps its words.
    pub fn sentence(&self) -> String {
        let sentence = self.steps_sentence();
        match self.plan_sentence() {
            Some(plan) => format!("{sentence} {plan}"),
            None => sentence,
        }
    }

    /// What the plan did with test commands of the lifecycle log that name a part of a suite, as a sentence for the card: the report
    /// chose a command for the whole suite in place of some, left some out with nothing in their place, or both, with the number of
    /// commands. The numbers are read from the detail lines [`TestReport::summary`] wrote ([`in_place_count`], [`left_out_count`]), so the
    /// summary has no member for them and every reader of a summary gives the same words. `None` for a report whose plan left nothing out.
    fn plan_sentence(&self) -> Option<String> {
        let (mut chosen, mut replaced, mut left_out) = (0usize, 0usize, 0usize);
        for line in &self.details {
            if let Some(commands) = in_place_count(line) {
                chosen += 1;
                replaced = replaced.saturating_add(commands);
            }
            left_out = left_out.saturating_add(left_out_count(line).unwrap_or(0));
        }
        let commands = |n: usize| match n {
            1 => "1 command of the lifecycle log that names part of a suite".to_string(),
            n => format!("{n} commands of the lifecycle log that name parts of a suite"),
        };
        let chose = match chosen {
            1 => "a whole-suite command",
            _ => "whole-suite commands",
        };
        match (replaced, left_out) {
            (0, 0) => None,
            (0, n) => Some(format!(
                "The report left out {}; details are in EVIDENCE.md.",
                commands(n)
            )),
            (n, 0) => Some(format!(
                "The report chose {chose} in place of {}; details are in EVIDENCE.md.",
                commands(n)
            )),
            (n, more) => Some(format!(
                "The report chose {chose} in place of {} and left out {more} more; details are in EVIDENCE.md.",
                commands(n)
            )),
        }
    }

    /// One plain sentence about the commands of the report: the claim when it is allowed, else what happened.
    fn steps_sentence(&self) -> String {
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
            return self
                .as_written_sentence(r)
                .or_else(|| self.choice_not_run_sentence(r))
                .unwrap_or_else(|| format!("{r}."));
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
            let head = format!("Setup `{failed}` exited {code} in a clean checkout");
            // The commands that a setup stopped, as the report lists them: those of the setup that failed first, and those of a
            // second setup that failed too. A command is counted with the setup its reason names; a reason that was cut names none and
            // counts for the first, as before.
            let first_setup = setup_in_reason(&failed);
            let parts = self.why().and_then(not_run_parts);
            let (stopped, by_another) = match parts {
                Some((command, why, more)) if why.starts_with(SKIPPED_FOR_SETUP) => {
                    match setup_named(why).filter(|setup| *setup != first_setup) {
                        Some(setup) => (None, Some((command, setup, more))),
                        None => (
                            Some(named(command, more)),
                            self.why().and_then(also_stopped),
                        ),
                    }
                }
                _ => (None, None),
            };
            let too = by_another
                .map(|(command, setup, more)| {
                    format!(
                        "; setup `{setup}` failed too, so {} did not run",
                        named(command, more)
                    )
                })
                .unwrap_or_default();
            if self.command.is_none() {
                // No test command ran. The sentence names the command the setup kept from running when the report lists one, as it
                // does below when other commands ran; a report that lists none keeps the words of before.
                return match (stopped, too.is_empty()) {
                    (Some(stopped), _) => format!("{head}, so {stopped} did not run{too}."),
                    (None, false) => format!("{head}{too}."),
                    (None, true) => format!("{head}, so the test command did not run."),
                };
            }
            // Test commands ran beside the install that failed: the sentence names the command it kept from running, or says that the
            // commands ran without it, and what the runner reported for those that ran.
            return match (stopped, too.is_empty(), parts) {
                (Some(stopped), ..) => format!(
                    "{head}, so {stopped} did not run{too}; for the test commands that ran, {}.",
                    self.counts_words()
                ),
                (None, false, _) => format!(
                    "{head}, so the test commands ran without it{too}; {}.",
                    self.counts_words()
                ),
                (None, true, Some(_)) => format!(
                    "{head}; for the test commands that ran, {}.",
                    self.counts_words()
                ),
                (None, true, None) => format!(
                    "{head}, so the test commands ran without it; {}.",
                    self.counts_words()
                ),
            };
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
