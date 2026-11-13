//! Which commands the clean-checkout report runs.
//!
//! The report runs the project's own test commands, never a command of the harness's choosing for a project that names none. Sources, in
//! this order, the first that gives a test command wins:
//!
//! 1. `.ham/test-command`, the project's own statement (one command per line; `#` comments; `setup: <command>` marks an install step);
//! 2. the lifecycle run log `.ham/lifecycle/LOG.md`: the test runs the engine logged, the last run of each command (a part of a suite is left
//!    out), so the report can set what the log said beside what a clean checkout gives. The notes a model writes after a command in the
//!    command column (`(3 tests)`, `# all green`, `exit 0`) are cut first (`command::strip_notes`), then a leading `timeout N`, the model's own
//!    limit on a run;
//! 3. the numbered checks of the goal card `.ham/lifecycle/GOAL.md`;
//!
//! The run log and the goal card sit where `.ham/records` says ([`Records`]): in the folder it names (`docs/lifecycle` for the value `docs`), in
//! `.ham/lifecycle` without the file, or nowhere when the repository turned the records off, in which case sources 2 and 3 give nothing and the plan goes on with source 4.
//! 4. a conservative reading of the manifests at the root of the tree (`scripts/test-all.sh`, pytest settings, `package.json`'s test
//!    script, `go.mod`, `Cargo.toml`, `pom.xml`, `build.gradle` with its wrapper), and of a plain Python layout with no settings at all: test
//!    files named `test_*.py` or `*_test.py` at the root or in `tests/` or `test/`. A file that imports pytest, or test functions with no unittest,
//!    give `python3 -m pytest -q`; files that all use unittest give `python3 -m unittest discover -v` (with `-s <folder>` when the folder is no
//!    package), one discovery, as a step of its own, for each place a discovery has to start in, so that every file is run; a file with a name
//!    the discovery's pattern `test*.py` leaves out (`*_test.py`) gives pytest, which collects it.
//!
//! A tree that gives none of these is `Plan::NoCommand` with the list of what the plan looked for ([`NO_COMMAND_REASON`]); a `.ham/test-command` file
//! names the command of any other layout.
//!
//! The install steps. A test command needs the packages its tree declares, and a clean checkout has none of them. A plan made from the manifests
//! (source 4) carries the install step it knows: the lockfile or `npm install` for a `package.json`, and for a Python tree with a
//! `requirements.txt` the report's own virtual environment ([`VENV_INSTALL`]), in which the tests then run. Test commands that come from the
//! lifecycle log or the goal card (sources 2 and 3) are the model's own lines and carry no install, so the plan puts the same install steps in
//! front of them. A plain Python layout whose packages are named by a file the report has no install step for
//! (`pyproject.toml`, `Pipfile`, `setup.py`, `setup.cfg`, a requirements file besides `requirements.txt`) is not run, and the reason says so:
//! the tests would fail for want of a step that is the report's own. `.ham/test-command` and the test
//! scripts of the repository are run as they are: their `setup:` lines and their own commands say what to install.
//!
//! Between the first source and the second the report reads the test scripts of the repository that the log or the goal card ran. A command
//! of the log or the card that runs a script of the tree (`bash scripts/test-e2e.sh`) is an entry point the repository declares. The report runs
//! those scripts as `bash <script>` from the root, in the order unit, integration, end to end, instead of the free-form runner commands the
//! model typed (`python3 -m pytest -q api/tests/integration` has no `pip install` behind it, which the script has). A script named `test-all`
//! stands for the others. A prefix of a command that names a program or tool folder the clean checkout does not have (`PYTHON=.venv/bin/python`)
//! is a session leftover and is left off, and a command that starts such a program is left out.
//!
//! The tree is the delivered head and its files are the model's: a source is read only when it is a regular file inside the tree
//! (a link that leaves it is ignored), at most 1 MiB, and a command is run by the unprivileged test user and recorded as written.

use crate::direct::records::{Records, TEST_COMMAND_FILES};
use crate::direct::report::command::{Parsed, classify};
use crate::direct::report::command::{strip_notes, strip_timeout};
use crate::direct::report::parse::{gotestsum_runs_tests, is_gotestsum_word};
use harness_protocol::direct::{
    LoggedRun, MAX_REPORT_COMMAND_CHARS, MAX_REPORT_STEPS, NO_COMMAND_FOUND_TESTS,
    PLAN_NOTE_COMMAND, TestCommandSource, TestStepRole,
};
use std::io::Read;
use std::path::Path;

/// One command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedStep {
    pub role: TestStepRole,
    pub source: TestCommandSource,
    pub command: String,
    /// For a command of the lifecycle log: what the log said about its last run.
    pub logged: Option<LoggedRun>,
}

/// The plan for a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    Steps(Vec<PlannedStep>),
    /// Nothing to run; the words say why.
    NoCommand(String),
}

/// Most test steps one plan holds.
pub const MAX_TEST_STEPS: usize = 12;

/// The largest source file read.
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;

/// Why a plan has no command: what it looked for, in the order it looked, and how a repository names the command of another layout.
pub const NO_COMMAND_REASON: &str = "it looked for .ham/test-command, test runs in the lifecycle log, numbered checks on the goal card, scripts/test-all.sh, pytest settings or test_*.py files, a package.json test script, go.mod, Cargo.toml, pom.xml and build.gradle with gradlew; a .ham/test-command file in the repository names the command";

/// The folders of a plain Python layout's tests, besides the root.
const PLAIN_TEST_DIRS: [&str; 2] = ["tests", "test"];
/// The install step of a Python tree with a `requirements.txt`: the report's own virtual environment in the checkout, with the packages of the
/// file. The environment also sees the packages of the image (`--system-site-packages`), pytest among them.
const VENV_INSTALL: &str = "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt";
/// The Python of that environment, as a step starts it from the root of the checkout.
const VENV_PYTHON: &str = ".ham-venv/bin/python";
/// The most test files of a plain layout that are read.
const MAX_PLAIN_TEST_FILES: usize = 64;

pub(crate) fn first_words(command: &str) -> Vec<Vec<&str>> {
    command
        .split("&&")
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split([';', '|']))
        .map(|seg| {
            let mut words: Vec<&str> = seg.split_whitespace().collect();
            // Leading `NAME=value` assignments are not the program.
            while words.first().is_some_and(|w| {
                w.split_once('=').is_some_and(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            }) {
                words.remove(0);
            }
            words
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether `command` starts a container tool (`docker`, `podman`, `docker-compose`, `podman-compose`, `nerdctl`) in any of its segments.
/// A file or an argument that only names one (`docker-compose.yml`) does not count.
pub fn uses_container_tools(command: &str) -> bool {
    first_words(command).iter().any(|w| {
        matches!(
            base(w[0]),
            "docker" | "podman" | "docker-compose" | "podman-compose" | "nerdctl"
        )
    })
}

/// Whether `go <args>` runs gotestsum to test: `go run <module>/gotestsum[@version] ...` (build flags may stand before the module) and `go tool gotestsum ...`
/// (no flag: `go tool -n gotestsum` only prints the tool's path), and the arguments after it are not `--version`, `tool slowest` and the like.
fn go_runs_gotestsum(args: &[&str]) -> bool {
    match args.first().copied() {
        Some("run") => args[1..]
            .iter()
            .position(|a| !a.starts_with('-'))
            .is_some_and(|p| {
                is_gotestsum_word(args[1 + p]) && gotestsum_runs_tests(&args[2 + p..])
            }),
        Some("tool") => {
            args.get(1).is_some_and(|a| is_gotestsum_word(a)) && gotestsum_runs_tests(&args[2..])
        }
        _ => false,
    }
}

/// Whether `command` runs a test runner the report knows.
pub fn is_test_command(command: &str) -> bool {
    first_words(command).iter().any(|w| {
        let (t0, t1, t2) = (
            base(w[0]),
            w.get(1).copied().unwrap_or(""),
            w.get(2).copied().unwrap_or(""),
        );
        let has = |x: &str| w.iter().skip(1).any(|t| *t == x);
        match t0 {
            "pytest" | "py.test" | "tox" | "jest" | "vitest" | "mocha" | "rspec" | "phpunit" => {
                true
            }
            "npm" | "pnpm" | "yarn" | "bun" => {
                t1 == "test" || t1 == "t" || (t1 == "run" && t2.starts_with("test"))
            }
            "npx" | "pnpx" => matches!(t1, "jest" | "vitest" | "mocha"),
            "node" => has("--test"),
            "go" => t1 == "test" || go_runs_gotestsum(&w[1..]),
            "gotestsum" => gotestsum_runs_tests(&w[1..]),
            "cargo" => t1 == "test" || t1 == "nextest",
            "mvn" | "mvnw" => has("test") || has("verify"),
            "gradle" | "gradlew" => has("test") || has("check"),
            "make" => t1 == "test" || t1 == "check",
            "dotnet" => t1 == "test",
            "bundle" => t1 == "exec" && t2 == "rspec",
            "sh" | "bash" => {
                let script = base(t1);
                script.starts_with("test") && script.ends_with(".sh")
            }
            t if t.starts_with("python") => {
                t1 == "-m" && matches!(t2, "pytest" | "unittest" | "tox")
            }
            t => t.starts_with("test") && t.ends_with(".sh"),
        }
    })
}

/// Whether `command` runs a part of a suite (a test id, a name filter, one file) and not the whole of it.
pub fn is_focused(command: &str) -> bool {
    first_words(command).iter().any(|w| {
        let is_cargo = base(w[0]) == "cargo";
        w.iter().skip(1).any(|t| {
            t.contains("::")
                || matches!(
                    *t,
                    "-k" | "-run"
                        | "--grep"
                        | "-t"
                        | "--testNamePattern"
                        | "--test-name-pattern"
                        | "--tests"
                )
                || t.starts_with("-Dtest=")
                || (is_cargo && *t == "--test")
                || t.ends_with(".py")
                || t.ends_with(".test.ts")
                || t.ends_with(".test.tsx")
                || t.ends_with(".test.js")
                || t.ends_with(".spec.ts")
                || t.ends_with(".spec.js")
                || t.ends_with("_test.go")
                || t.ends_with("Test.java")
        })
    })
}

/// A `RUN | <command> | exit <code> | pass <n or unknown> fail <n or unknown> | <summary>` line of the lifecycle log.
pub fn parse_log_line(line: &str) -> Option<(String, LoggedRun)> {
    let rest = line.trim().strip_prefix("RUN | ")?;
    // The command may hold a pipe of its own: read from the right for the exit field that is followed by the count fields.
    for (idx, _) in rest.rmatch_indices(" | exit ") {
        let command = rest[..idx].trim();
        if command.is_empty() {
            continue;
        }
        let after = &rest[idx + " | exit ".len()..];
        let mut parts = after.splitn(3, " | ");
        let Some(code) = parts.next().and_then(|c| c.trim().parse::<i32>().ok()) else {
            continue;
        };
        let Some(counts) = parts.next().map(str::trim) else {
            continue;
        };
        let Some(counts) = counts.strip_prefix("pass ") else {
            continue;
        };
        let Some((pass, fail)) = counts.split_once(" fail ") else {
            continue;
        };
        let num = |s: &str| s.trim().parse::<u32>().ok();
        return Some((
            command.to_string(),
            LoggedRun {
                exit_code: Some(code),
                passed: num(pass),
                failed: num(fail),
            },
        ));
    }
    None
}

/// The text of `rel` under `root`, when it is a regular file that stays inside the tree.
fn read_source(root: &Path, rel: &str) -> Option<String> {
    let real = root.join(rel).canonicalize().ok()?;
    if !real.starts_with(root) || !real.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&real)
        .ok()?
        .take(MAX_SOURCE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn has_file(root: &Path, rel: &str) -> bool {
    root.join(rel)
        .canonicalize()
        .is_ok_and(|p| p.starts_with(root) && p.is_file())
}

fn step(role: TestStepRole, source: TestCommandSource, command: &str) -> PlannedStep {
    PlannedStep {
        role,
        source,
        command: command.to_string(),
        logged: None,
    }
}

fn from_command_file(root: &Path) -> Vec<PlannedStep> {
    // `.ham/test-command`, else the file of the earlier folder (COMPAT-CAH-DATA).
    let Some(text) = TEST_COMMAND_FILES
        .iter()
        .find_map(|file| read_source(root, file))
    else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    let mut tests = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (role, command) = match line.strip_prefix("setup:") {
            Some(c) => (TestStepRole::Setup, c.trim()),
            None => (TestStepRole::Test, line),
        };
        if command.is_empty()
            || command.chars().count() > MAX_REPORT_COMMAND_CHARS
            || command.chars().any(char::is_control)
        {
            continue;
        }
        if role == TestStepRole::Test {
            if tests == MAX_TEST_STEPS {
                continue;
            }
            tests += 1;
        }
        steps.push(step(role, TestCommandSource::TestCommandFile, command));
    }
    if tests == 0 {
        return Vec::new();
    }
    steps.truncate(MAX_REPORT_STEPS);
    steps
}

fn plain_command(c: &str) -> bool {
    !c.is_empty()
        && c.chars().count() <= MAX_REPORT_COMMAND_CHARS
        && !c.chars().any(char::is_control)
        && is_test_command(c)
        && !is_focused(c)
}

/// Whether `value` names a place in the tree that a clean checkout does not have: a relative path (it holds a `/`, does not start with one and has no
/// variable in it) with nothing at that place. A `PATH`-style list is not read (a `:` in it).
fn missing_relative_path(root: &Path, value: &str) -> bool {
    value.contains('/')
        && !value.starts_with('/')
        && !value.contains(['$', ':', '~'])
        && !root.join(value).exists()
}

/// A word as a shell reads it back: plain words stay, others go in single quotes.
fn shell_word(w: &str) -> String {
    if !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// A command of the log or the goal card as the clean checkout can run it: `None` when it starts a program by a relative path that the tree does
/// not have (`.venv/bin/python -m pytest`), which only the model's session had; the command without its `NAME=value` prefixes whose value is such a
/// program or tool folder (`PYTHON=.venv/bin/python bash scripts/test-e2e.sh` becomes `bash scripts/test-e2e.sh`); else the command as it is. Only a command of plain
/// words is changed: anything the shell has to read (`cd api && cmd`) is left as it is.
fn fit_for_checkout(root: &Path, command: &str) -> Option<String> {
    let Ok(Parsed::Plain { env, argv }) = classify(command) else {
        return Some(command.to_string());
    };
    if argv.first().is_some_and(|p| missing_relative_path(root, p)) {
        return None;
    }
    // A prefix is a session leftover when its value is a program or a tool folder (`.venv/bin/python`, `node_modules/.bin`) that the tree does not
    // have. A value that names a file the command is going to write (`reports/junit.xml`) is not.
    let leftover =
        |v: &str| (v.contains("/bin/") || v.contains("/.bin")) && missing_relative_path(root, v);
    if !env.iter().any(|(_, v)| leftover(v)) {
        return Some(command.to_string());
    }
    let mut words: Vec<String> = env
        .iter()
        .filter(|(_, v)| !leftover(v))
        .map(|(k, v)| format!("{k}={}", shell_word(v)))
        .collect();
    words.extend(argv.iter().map(|w| shell_word(w)));
    Some(words.join(" "))
}

/// The script of the tree that `command` runs: `bash scripts/test-e2e.sh`, `sh scripts/x.sh -v` or `./scripts/test-all.sh`, as the path under the root.
/// `None` for any command that is not one plain word list starting a file of the tree.
fn script_of(root: &Path, command: &str) -> Option<String> {
    let Ok(Parsed::Plain { argv, .. }) = classify(command) else {
        return None;
    };
    let first = argv.first()?;
    let script = if matches!(base(first), "bash" | "sh") {
        argv.iter().skip(1).find(|a| !a.starts_with('-'))?.clone()
    } else {
        first.clone()
    };
    let script = script.trim_start_matches("./").to_string();
    (script.ends_with(".sh") && has_file(root, &script)).then_some(script)
}

/// The order of the test scripts: unit first, then integration, then end to end, then the rest; by name inside each.
fn script_rank(script: &str) -> (u8, String) {
    let name = base(script).to_ascii_lowercase();
    let rank = if name.contains("unit") {
        0
    } else if name.contains("integration") {
        1
    } else if name.contains("e2e") || name.contains("end-to-end") {
        2
    } else {
        3
    };
    (rank, script.to_string())
}

/// A script that stands for the others: `test-all.sh`.
fn is_aggregate(script: &str) -> bool {
    matches!(
        base(script).to_ascii_lowercase().replace('_', "-").as_str(),
        "test-all.sh" | "tests-all.sh" | "test-everything.sh"
    )
}

/// Every plain test command of the lifecycle log made fit for the clean checkout, in the order of each command's last run.
fn logged_commands(root: &Path, records: &Records) -> Vec<(String, LoggedRun)> {
    let Some(text) = records.log().and_then(|rel| read_source(root, &rel)) else {
        return Vec::new();
    };
    let mut seen: Vec<(String, LoggedRun)> = Vec::new();
    for line in text.lines() {
        let Some((column, logged)) = parse_log_line(line) else {
            continue;
        };
        // The log has the exit code and the counts in columns of their own, so text after the command in this column is a note. A
        // `timeout N` in front is the model's own limit on the run and not part of the command: the report gives every step its limit.
        let command = strip_timeout(&strip_notes(&column));
        if !plain_command(&command) {
            continue;
        }
        let Some(command) = fit_for_checkout(root, &command) else {
            continue;
        };
        seen.retain(|(c, _)| *c != command);
        seen.push((command, logged));
    }
    seen
}

/// The test scripts of the repository that the log or the goal card ran, as `bash <script>` steps in the order of [`script_rank`]; a
/// script named `test-all` stands for the others. Empty when neither source ran a script of the tree.
fn entry_scripts(root: &Path, records: &Records) -> Vec<PlannedStep> {
    let mut found: Vec<(String, String, TestCommandSource, Option<LoggedRun>)> = Vec::new();
    let from_log = logged_commands(root, records);
    let from_goal = goal_commands(root, records);
    // The log's commands are in the order of each one's last run, so the walk from the end meets the latest run of a script first, whatever the
    // spelling of that run (`CI=1 bash scripts/x.sh` or the bare form): the step and the row beside it are the model's last word on the script
    // and not a first attempt made before the script existed. The goal card comes after the log.
    let all = from_log
        .iter()
        .rev()
        .map(|(c, l)| (c.clone(), TestCommandSource::LifecycleLog, Some(*l)))
        .chain(
            from_goal
                .iter()
                .map(|c| (c.clone(), TestCommandSource::GoalCard, None)),
        );
    for (command, source, logged) in all {
        let Some(script) = script_of(root, &command) else {
            continue;
        };
        if found.iter().any(|(s, ..)| *s == script) {
            continue;
        }
        // The command keeps the prefixes that are left (`CI=1`), the interpreter the model used and no arguments: a focused run is the model's.
        let Ok(Parsed::Plain { env, argv }) = classify(&command) else {
            continue;
        };
        let runner = if matches!(base(&argv[0]), "sh") {
            "sh"
        } else {
            "bash"
        };
        let mut words: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("{k}={}", shell_word(v)))
            .collect();
        words.push(runner.to_string());
        words.push(shell_word(&script));
        found.push((script, words.join(" "), source, logged));
    }
    if let Some(all) = found.iter().position(|(s, ..)| is_aggregate(s)) {
        let keep = found.remove(all);
        found = vec![keep];
    }
    found.sort_by_key(|(s, ..)| script_rank(s));
    found.truncate(MAX_TEST_STEPS);
    found
        .into_iter()
        .map(|(_, command, source, logged)| PlannedStep {
            logged,
            ..step(TestStepRole::Test, source, &command)
        })
        .collect()
}

fn from_log(root: &Path, records: &Records) -> Vec<PlannedStep> {
    let mut seen = logged_commands(root, records);
    let keep = seen.len().saturating_sub(MAX_TEST_STEPS);
    seen.drain(..keep);
    seen.into_iter()
        .map(|(command, logged)| PlannedStep {
            logged: Some(logged),
            ..step(
                TestStepRole::Test,
                TestCommandSource::LifecycleLog,
                &command,
            )
        })
        .collect()
}

/// Every plain test command among the numbered checks of the goal card, made fit for the clean checkout, once each.
fn goal_commands(root: &Path, records: &Records) -> Vec<String> {
    let Some(text) = records.goal().and_then(|rel| read_source(root, &rel)) else {
        return Vec::new();
    };
    let mut commands: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || !line[digits..].starts_with('.') {
            continue;
        }
        let Some(open) = line.find('`') else {
            continue;
        };
        let Some(len) = line[open + 1..].find('`') else {
            continue;
        };
        let command = line[open + 1..open + 1 + len].trim();
        if plain_command(command)
            && let Some(command) = fit_for_checkout(root, command)
            && !commands.contains(&command)
        {
            commands.push(command);
        }
    }
    commands
}

fn from_goal(root: &Path, records: &Records) -> Vec<PlannedStep> {
    let mut commands = goal_commands(root, records);
    commands.truncate(MAX_TEST_STEPS);
    commands
        .iter()
        .map(|c| step(TestStepRole::Test, TestCommandSource::GoalCard, c))
        .collect()
}

fn detect(root: &Path) -> Vec<PlannedStep> {
    use TestCommandSource::Detected;
    use TestStepRole::{Setup, Test};
    if has_file(root, "scripts/test-all.sh") {
        return vec![step(Test, Detected, "sh scripts/test-all.sh")];
    }
    let mut steps = Vec::new();
    // The arguments of the Python test commands: pytest for a tree with pytest settings, else what a plain layout gives, unless a file the report
    // has no install step for names the packages of the project.
    let commands = if python_settings(root) {
        vec!["-m pytest -q".to_string()]
    } else if uninstalled_python_manifest(root).is_some() {
        Vec::new()
    } else {
        plain_python_layout(root)
    };
    if !commands.is_empty() {
        // The test steps come with the install step their tree needs: the packages of requirements.txt, in the environment the tests run in.
        let python = if has_file(root, "requirements.txt") {
            steps.push(step(Setup, Detected, VENV_INSTALL));
            VENV_PYTHON
        } else {
            "python3"
        };
        for args in &commands {
            steps.push(step(Test, Detected, &format!("{python} {args}")));
        }
    }
    if let Some(pkg) = read_source(root, "package.json")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg)
        && let Some(test) = v.pointer("/scripts/test").and_then(|t| t.as_str())
        && !test.contains("no test specified")
    {
        if has_file(root, "pnpm-lock.yaml") {
            steps.push(step(Setup, Detected, "pnpm install --frozen-lockfile"));
            steps.push(step(Test, Detected, "pnpm test"));
        } else {
            let install = if has_file(root, "package-lock.json") {
                "npm ci"
            } else {
                "npm install"
            };
            steps.push(step(Setup, Detected, install));
            steps.push(step(Test, Detected, "npm test"));
        }
    }
    if has_file(root, "go.mod") {
        steps.push(step(Test, Detected, "go test -v ./..."));
    }
    if has_file(root, "Cargo.toml") {
        steps.push(step(Test, Detected, "cargo test"));
    }
    if has_file(root, "pom.xml") {
        let mvn = if has_file(root, "mvnw") {
            "./mvnw -B test"
        } else {
            "mvn -B test"
        };
        steps.push(step(Test, Detected, mvn));
    }
    if (has_file(root, "build.gradle") || has_file(root, "build.gradle.kts"))
        && has_file(root, "gradlew")
    {
        steps.push(step(Test, Detected, "./gradlew test"));
    }
    steps.truncate(MAX_REPORT_STEPS);
    steps
}

/// Whether the tree has pytest settings: a `pytest.ini` or a `conftest.py` at the root, or a settings or requirements file that names pytest.
fn python_settings(root: &Path) -> bool {
    let mentions = |rel: &str, word: &str| read_source(root, rel).is_some_and(|t| t.contains(word));
    has_file(root, "pytest.ini")
        || has_file(root, "conftest.py")
        || mentions("tox.ini", "pytest")
        || mentions("setup.cfg", "[tool:pytest]")
        || mentions("pyproject.toml", "pytest")
        || mentions("requirements.txt", "pytest")
        || mentions("requirements-dev.txt", "pytest")
}

/// Whether the TOML `text` names a package: a string in the array of one of the keys `arrays` (`project.dependencies`) or of a key under one
/// (an extra of `project.optional-dependencies`, a group of `dependency-groups`), or a key other than `python` under one of `tables`
/// (`tool.poetry.dependencies`, the `packages` of a Pipfile), in which a `*` stands for one name (`tool.poetry.group.*.dependencies`). A key is
/// read with its table: `dependencies` under `[project]` and the dotted key `project.dependencies` are the same key. The value of every key is
/// read to the end of its brackets, so a line of an array that starts with a bracket is not taken for a table header. When an array of
/// `arrays` never closes it is read as naming a package: the reading is not sure, and tests run without their packages would fail for a reason
/// that is the report's own. Any other value that never closes ends the reading: nothing after it can be told apart.
fn names_packages(text: &str, arrays: &[&str], tables: &[&str]) -> bool {
    let mut table = String::new();
    let mut lines = text.lines();
    while let Some(raw) = lines.next() {
        let line = without_comment(raw).unwrap_or(raw).trim();
        if let Some(header) = line.strip_prefix('[') {
            table = header
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        let path = if table.is_empty() {
            key.to_string()
        } else {
            format!("{table}.{key}")
        };
        // The key that follows the table at once, or, for a table written `head*tail`, after one name between the two parts.
        let package_of = |t: &&str| {
            let (head, tail) = t.split_once('*').unwrap_or((*t, ""));
            path.strip_prefix(head)
                .and_then(|rest| rest.split_once(&format!("{tail}.")))
                .is_some_and(|(name, key)| {
                    name.is_empty() == tail.is_empty() && !name.contains('.') && key != "python"
                })
        };
        if tables.iter().any(package_of) {
            return true;
        }
        let wanted = arrays.iter().any(|a| {
            path.strip_prefix(*a)
                .is_some_and(|k| k.is_empty() || k.starts_with('.'))
        });
        // The value may run over several lines: read on until its brackets close.
        let mut buf = value.trim().to_string();
        loop {
            match array_state(&buf) {
                Some((depth, strings)) if depth <= 0 => {
                    if wanted && !strings.is_empty() {
                        return true;
                    }
                    break;
                }
                _ => {
                    let Some(next) = lines.next() else {
                        return wanted;
                    };
                    buf.push(' ');
                    buf.push_str(without_comment(next).unwrap_or(next).trim());
                }
            }
        }
    }
    false
}

/// Whether a `setup.py` names a package under `install_requires`: the key is given a list that holds a string, or something that is no
/// list (a variable, a call), which the reading does not follow and takes for packages. An empty list names none, and neither does the word
/// in a comment line.
fn setup_py_names_packages(text: &str) -> bool {
    let code: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    code.match_indices("install_requires").any(|(at, word)| {
        let Some(value) = code[at + word.len()..].trim_start().strip_prefix('=') else {
            return false;
        };
        match value.trim_start().strip_prefix('[') {
            Some(list) => list
                .split(']')
                .next()
                .is_some_and(|inner| inner.contains(['\'', '"'])),
            None => true,
        }
    })
}

/// Whether a `setup.cfg` names a package under `install_requires`: a value on the line of the key, or on the indented lines after it
/// (comment lines aside).
fn setup_cfg_names_packages(text: &str) -> bool {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(value) = line
            .trim_start()
            .strip_prefix("install_requires")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        else {
            continue;
        };
        return !value.trim().is_empty()
            || lines
                .by_ref()
                .take_while(|l| l.starts_with([' ', '\t']))
                .map(str::trim)
                .any(|l| !l.is_empty() && !l.starts_with(['#', ';']));
    }
    false
}

/// The requirements files of the tree besides `requirements.txt`, as paths relative to the root: the files at the root whose name holds
/// `requirements` and ends in `.txt` (`requirements-dev.txt`, `dev-requirements.txt`, `test-requirements.txt`, `requirements-test.txt`), then
/// the `.txt` files of the folder `requirements` (`requirements/dev.txt`), each in the order of the names; at most [`MAX_PLAIN_TEST_FILES`].
fn other_requirements(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in ["", "requirements"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".txt") && (!dir.is_empty() || n.contains("requirements")))
            .collect();
        names.sort();
        found.extend(names.into_iter().map(|n| match dir {
            "" => n,
            _ => format!("{dir}/{n}"),
        }));
    }
    found.truncate(MAX_PLAIN_TEST_FILES);
    found
}

/// The file that names the packages of a Python project and that the report has no install step for: `pyproject.toml` with a package in the
/// `[project]` `dependencies` array, in an extra of `[project.optional-dependencies]` or in a group of `[dependency-groups]`, or with an entry
/// besides `python` under `[tool.poetry.dependencies]` or a `[tool.poetry.group.<name>.dependencies]`; a `Pipfile` with an entry under
/// `[packages]` or `[dev-packages]`; a `setup.py` or `setup.cfg` whose `install_requires` names one ([`setup_py_names_packages`],
/// [`setup_cfg_names_packages`]); else the first requirements file besides `requirements.txt` ([`other_requirements`]) with a line that is
/// neither empty nor a comment (a line that points at another file counts: the reading does not follow it). `None` for a tree with a
/// `requirements.txt`, which the report installs, and for a tree where none of these files names a package.
fn uninstalled_python_manifest(root: &Path) -> Option<String> {
    if has_file(root, "requirements.txt") {
        return None;
    }
    let text = |rel: &str| read_source(root, rel);
    if text("pyproject.toml").is_some_and(|t| {
        names_packages(
            &t,
            &[
                "project.dependencies",
                "project.optional-dependencies",
                "dependency-groups",
            ],
            &[
                "tool.poetry.dependencies",
                "tool.poetry.group.*.dependencies",
            ],
        )
    }) {
        return Some("pyproject.toml".into());
    }
    if text("Pipfile").is_some_and(|t| names_packages(&t, &[], &["packages", "dev-packages"])) {
        return Some("Pipfile".into());
    }
    if text("setup.py").is_some_and(|t| setup_py_names_packages(&t)) {
        return Some("setup.py".into());
    }
    if text("setup.cfg").is_some_and(|t| setup_cfg_names_packages(&t)) {
        return Some("setup.cfg".into());
    }
    other_requirements(root).into_iter().find(|rel| {
        text(rel).is_some_and(|t| {
            t.lines()
                .map(str::trim)
                .any(|l| !l.is_empty() && !l.starts_with('#'))
        })
    })
}

/// Why the test files of a plain Python layout were found and not run, for a tree that gave no test command (so it has no pytest
/// settings: those always give one): the tree has test files ([`plain_python_test_files`]), and a file names the packages of the project that
/// the report has no install step for ([`uninstalled_python_manifest`]). Run without those packages the tests would fail, and the failure
/// would be the report's own. The words start with [`NO_COMMAND_FOUND_TESTS`], so the card's sentence says that the tests were not run and why.
fn plain_layout_not_run(root: &Path) -> Option<String> {
    let manifest = uninstalled_python_manifest(root)?;
    let files = plain_python_test_files(root);
    let first = files.first()?;
    let more = match files.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(format!(
        "{NO_COMMAND_FOUND_TESTS}Python test files ({first}{more}), and {manifest} names packages that the report does not install (it installs only the packages of requirements.txt); a .ham/test-command file in the repository names the install and test commands"
    ))
}

/// Whether `name` is the name of a Python test file: `test_*.py` or `*_test.py`.
fn python_test_name(name: &str) -> bool {
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

/// The test files of a plain Python layout under `root`, as paths relative to it: at the root, then in each of [`PLAIN_TEST_DIRS`]. A file must be a
/// regular file inside the tree; at most [`MAX_PLAIN_TEST_FILES`].
fn plain_python_test_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in std::iter::once("").chain(PLAIN_TEST_DIRS) {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| python_test_name(n))
            .collect();
        names.sort();
        for name in names {
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if has_file(root, &rel) && found.len() < MAX_PLAIN_TEST_FILES {
                found.push(rel);
            }
        }
    }
    found
}

/// The arguments of the test commands of a plain Python layout with no settings, as they follow the interpreter: `-m pytest -q`, or for
/// files that all use unittest the discoveries that find them all, one command each ([`unittest_discoveries`]). Empty when the tree has no
/// Python test file.
fn plain_python_layout(root: &Path) -> Vec<String> {
    let files = plain_python_test_files(root);
    if files.is_empty() {
        return Vec::new();
    }
    let texts: Vec<String> = files
        .iter()
        .map(|f| read_source(root, f).unwrap_or_default())
        .collect();
    let pytest = texts
        .iter()
        .any(|t| t.contains("import pytest") || t.contains("from pytest"));
    let unittest = texts.iter().all(|t| t.contains("unittest"));
    if !pytest
        && unittest
        && let Some(discoveries) = unittest_discoveries(root, &files)
    {
        return discoveries;
    }
    // pytest collects `test_*.py` and `*_test.py` in every folder, the classes of unittest among them.
    vec!["-m pytest -q".to_string()]
}

/// The arguments of the unittest discoveries that together find every file of `files` (paths under `root`, grouped by folder), one command
/// each. A discovery finds the files named `test*.py` in its start folder and in the packages under it. So one discovery starts at the
/// root when a file sits at the root or in a folder that is a package, and each folder that is no package gets a discovery that starts in it
/// (the root stays on the import path, so the tests still import the project's modules). The files stay with the runner they were written
/// for: pytest would import two files of one name in two folders as one module. `None` when a file has a name the pattern leaves out
/// (`*_test.py`): no discovery finds it.
fn unittest_discoveries(root: &Path, files: &[String]) -> Option<Vec<String>> {
    if !files.iter().all(|f| base(f).starts_with("test")) {
        return None;
    }
    let mut folders: Vec<&str> = files
        .iter()
        .map(|f| f.split_once('/').map_or("", |(d, _)| d))
        .collect();
    folders.dedup();
    let loose = |d: &&str| !d.is_empty() && !has_file(root, &format!("{d}/__init__.py"));
    let mut discoveries = Vec::new();
    if !folders.iter().all(loose) {
        discoveries.push("-m unittest discover -v".to_string());
    }
    discoveries.extend(
        folders
            .iter()
            .filter(|d| loose(d))
            .map(|d| format!("-m unittest discover -s {d} -v")),
    );
    Some(discoveries)
}

// ---- the install steps of commands that came from the log or the goal card ----------------------------------------------------

/// Whether `command` starts a Node package tool or test runner in one of its segments.
fn runs_node(command: &str) -> bool {
    first_words(command).iter().any(|w| {
        matches!(
            base(w[0]),
            "npm" | "pnpm" | "yarn" | "bun" | "npx" | "pnpx" | "node" | "jest" | "vitest" | "mocha"
        )
    })
}

/// Whether `command` works in the root of the checkout: none of its segments changes the folder (`cd web && npm test`, `( cd web && npm test )`)
/// and none tells its tool to work in another one (`npm test --prefix web`, `pnpm -C web test`, `yarn --cwd web test`).
fn works_at_root(command: &str) -> bool {
    !first_words(command).iter().any(|w| {
        let program = w
            .iter()
            .map(|t| t.trim_start_matches('('))
            .find(|t| !t.is_empty());
        matches!(program, Some("cd" | "pushd"))
            || w.iter().any(|t| {
                matches!(*t, "--prefix" | "-C" | "--cwd" | "--dir")
                    || ["--prefix=", "--cwd=", "--dir="]
                        .iter()
                        .any(|option| t.starts_with(option))
            })
    })
}

/// The install step of the Node packages at the root of the tree, for test commands that came from the log or the goal card: from the lockfile
/// when the tree has one (`pnpm-lock.yaml`, then `package-lock.json`), else `npm install` when `package.json` names a package. `None` for a tree
/// with no `package.json` at its root, for one with no lockfile whose manifest names no package or cannot be read, and for one whose only
/// lockfile is of a tool the report has no install step for (`yarn.lock`, `bun.lock`, `bun.lockb`): such a tree may carry its packages itself,
/// and `npm install` is not its install.
fn node_install(root: &Path) -> Option<&'static str> {
    let manifest = read_source(root, "package.json")?;
    if has_file(root, "pnpm-lock.yaml") {
        return Some("pnpm install --frozen-lockfile");
    }
    if has_file(root, "package-lock.json") {
        return Some("npm ci");
    }
    if ["yarn.lock", "bun.lock", "bun.lockb"]
        .iter()
        .any(|lockfile| has_file(root, lockfile))
    {
        return None;
    }
    let manifest = serde_json::from_str::<serde_json::Value>(&manifest).ok()?;
    ["dependencies", "devDependencies", "optionalDependencies"]
        .iter()
        .any(|k| {
            manifest
                .get(k)
                .and_then(|d| d.as_object())
                .is_some_and(|d| !d.is_empty())
        })
        .then_some("npm install")
}

/// A test command of the log or the goal card as it runs in the report's virtual environment: the words of a plain command that starts `python`,
/// `python3` or `python3.N` with `-m pytest` or `-m unittest`, with the environment's Python in the program's place, and `pytest` or `py.test`
/// as that Python's `-m pytest`. The `NAME=value` prefixes stay. `None` for anything else: a command the shell has to read, a program started by
/// a path, tox (it makes its own environments).
fn in_venv(command: &str) -> Option<String> {
    let Ok(Parsed::Plain { env, argv }) = classify(command) else {
        return None;
    };
    let (program, rest) = argv.split_first()?;
    let python = program == "python"
        || program == "python3"
        || program
            .strip_prefix("python3.")
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
    let module = rest.first().is_some_and(|w| w == "-m")
        && rest
            .get(1)
            .is_some_and(|m| m == "pytest" || m == "unittest");
    let mut words: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", shell_word(v)))
        .collect();
    words.push(VENV_PYTHON.to_string());
    if program == "pytest" || program == "py.test" {
        words.extend(["-m".to_string(), "pytest".to_string()]);
    } else if !(python && module) {
        return None;
    }
    words.extend(rest.iter().map(|w| shell_word(w)));
    Some(words.join(" "))
}

/// The steps of test commands that came from the lifecycle log or the goal card, with the install steps their tree needs in front. The
/// commands are the model's own lines and carry no install, and a clean checkout has no `node_modules` and none of the packages the session
/// installed: `npm test` then fails to load a test file of code whose tests pass. The install steps are the ones the detection knows, each for
/// the commands it serves:
///
/// - a command that starts a Node tool at the root gets the install of the lockfile, or of the packages `package.json` names ([`node_install`]);
/// - a plain Python command at the root of a tree with a `requirements.txt` runs in the report's virtual environment ([`in_venv`]), which
///   [`VENV_INSTALL`] makes first.
///
/// A command that changes the folder, a command of another runner and a Python command the shell has to read stay as they are and bring no
/// install. An install step that fails ends the steps, and the test commands behind it are listed as not run (`report::run_steps`).
fn with_installs(root: &Path, mut steps: Vec<PlannedStep>) -> Vec<PlannedStep> {
    use TestCommandSource::Detected;
    use TestStepRole::Setup;
    let mut out = Vec::new();
    if steps
        .iter()
        .any(|s| works_at_root(&s.command) && runs_node(&s.command))
        && let Some(install) = node_install(root)
    {
        out.push(step(Setup, Detected, install));
    }
    if has_file(root, "requirements.txt") {
        let mut moved = false;
        for s in &mut steps {
            if let Some(command) = in_venv(&s.command) {
                s.command = command;
                moved = true;
            }
        }
        if moved {
            out.push(step(Setup, Detected, VENV_INSTALL));
        }
    }
    out.append(&mut steps);
    out
}

/// The plan for the tree at `tree`.
pub fn plan(tree: &Path) -> Plan {
    let Ok(root) = tree.canonicalize() else {
        return Plan::NoCommand("the clean checkout cannot be read".into());
    };
    let records = Records::read(&root);
    // Each source with whether its commands are the model's own lines, which carry no install step (the log and the goal card).
    for (steps, typed_by_the_model) in [
        (from_command_file(&root), false),
        (entry_scripts(&root, &records), false),
        (from_log(&root, &records), true),
        (from_goal(&root, &records), true),
        (detect(&root), false),
    ] {
        if steps.iter().any(|s| s.role == TestStepRole::Test) {
            return Plan::Steps(if typed_by_the_model {
                with_installs(&root, steps)
            } else {
                steps
            });
        }
    }
    Plan::NoCommand(plain_layout_not_run(&root).unwrap_or_else(|| NO_COMMAND_REASON.into()))
}

// ---- how many members `cargo test` runs at the root -------------------------------------------------------------------------

/// The parts of a `Cargo.toml` that decide what a bare `cargo test` at its folder runs.
#[derive(Debug, Default)]
struct CargoManifest {
    /// The manifest has a `[package]` (or a dotted `package.` key): cargo then tests that package alone unless `default-members` says more.
    package: bool,
    /// The manifest has a `[workspace]` table (or a dotted `workspace.` key).
    workspace: bool,
    members: Vec<String>,
    default_members: Option<Vec<String>>,
    exclude: Vec<String>,
}

/// `line` without a comment: everything from the first `#` that is not inside a string. `None` for a string that does not end on the line.
fn without_comment(line: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '#' => return Some(&line[..i]),
                _ => {}
            },
        }
    }
    quote.is_none().then_some(line)
}

/// How deep the brackets of `text` are left open, ignoring brackets inside strings; the strings of `text` as well. `None` for a string that does not end.
fn array_state(text: &str) -> Option<(i32, Vec<String>)> {
    let mut depth = 0i32;
    let mut strings = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            '"' | '\'' => {
                let mut value = String::new();
                loop {
                    match chars.next()? {
                        '\\' if c == '"' => value.push(chars.next()?),
                        q if q == c => break,
                        other => value.push(other),
                    }
                }
                strings.push(value);
            }
            _ => {}
        }
    }
    Some((depth, strings))
}

impl CargoManifest {
    /// The reading of `text`, or `None` when it is not sure what the text says (an unclosed header, string or array).
    fn read(text: &str) -> Option<Self> {
        #[derive(PartialEq)]
        enum Table {
            Root,
            Package,
            Workspace,
            Other,
        }
        let mut manifest = Self::default();
        let mut table = Table::Root;
        let mut lines = text.lines();
        while let Some(raw) = lines.next() {
            let line = without_comment(raw)?.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with("[[") {
                table = Table::Other;
                continue;
            }
            if let Some(header) = line.strip_prefix('[') {
                let name = header.split_once(']')?.0.trim();
                if name == "package" || name.starts_with("package.") {
                    manifest.package = true;
                }
                if name == "workspace" || name.starts_with("workspace.") {
                    manifest.workspace = true;
                }
                table = match name {
                    "package" => Table::Package,
                    "workspace" => Table::Workspace,
                    _ => Table::Other,
                };
                continue;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            // The value may run over several lines (an array of any key, also of a table this reading does not use): read on until its brackets
            // close.
            let mut buf = value.trim().to_string();
            let mut state = array_state(&buf)?;
            while state.0 > 0 {
                let next = without_comment(lines.next()?)?.trim().to_string();
                buf.push(' ');
                buf.push_str(&next);
                state = array_state(&buf)?;
            }
            if state.0 < 0 {
                return None;
            }
            // A key at the top may name its table with a dotted key: `workspace.members = [...]`, `package.name = "x"`.
            let key = match (&table, key) {
                (Table::Root, k) if k == "package" || k.starts_with("package.") => {
                    manifest.package = true;
                    continue;
                }
                (Table::Root, k) if k.starts_with("workspace.") => {
                    manifest.workspace = true;
                    k.trim_start_matches("workspace.").trim()
                }
                (Table::Workspace, k) => k,
                _ => continue,
            };
            if !matches!(key, "members" | "default-members" | "exclude") {
                continue;
            }
            if !buf.starts_with('[') {
                return None;
            }
            match key {
                "members" => manifest.members = state.1,
                "default-members" => manifest.default_members = Some(state.1),
                _ => manifest.exclude = state.1,
            }
        }
        Some(manifest)
    }
}

/// A member path as cargo compares it: no leading `./`, no trailing `/`. `None` for a path that is absolute or leaves its folder with `..`.
fn member_path(raw: &str) -> Option<String> {
    let path = raw.trim().trim_start_matches("./").trim_end_matches('/');
    let path = if path.is_empty() { "." } else { path };
    (!path.starts_with('/') && !path.split('/').any(|part| part == "..")).then(|| path.to_string())
}

/// The member folders `entry` names under `root`: the folder itself, or for `dir/*` (a star as the whole last component) every folder under `dir`
/// that holds a `Cargo.toml`. `None` for a pattern this reading does not know.
fn expand_member(root: &Path, entry: &str) -> Option<Vec<String>> {
    let has_pattern = |s: &str| s.contains(['*', '?', '[', '{']);
    if !has_pattern(entry) {
        return Some(vec![entry.to_string()]);
    }
    let prefix = if entry == "*" {
        ""
    } else {
        entry.strip_suffix("/*")?
    };
    if has_pattern(prefix) {
        return None;
    }
    let dir = root.join(prefix).canonicalize().ok()?;
    if !dir.starts_with(root) {
        return Some(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .take(10_000)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Some(
        names
            .into_iter()
            .map(|n| {
                if prefix.is_empty() {
                    n
                } else {
                    format!("{prefix}/{n}")
                }
            })
            .collect(),
    )
}

/// How many members a bare `cargo test` runs at the root of `root`, when that is more than one. A virtual manifest runs its
/// `default-members` when it names some and else all of `members`; a manifest with a `[package]` runs that package alone unless `default-members`
/// says more. A member counts when its folder is inside the tree and holds a `Cargo.toml`; `exclude` takes folders out; a member listed twice counts
/// once. `None` for a tree that is no workspace, for one member or none, and for anything the reading is not sure of (a manifest it cannot read, a
/// pattern other than `dir/*`), so the report never states a number it did not read.
pub fn cargo_workspace_members(root: &Path) -> Option<u32> {
    let root = root.canonicalize().ok()?;
    let manifest = CargoManifest::read(&read_source(&root, "Cargo.toml")?)?;
    if !manifest.workspace {
        return None;
    }
    let listed = match (&manifest.default_members, manifest.package) {
        (Some(defaults), _) => defaults.clone(),
        (None, true) => return None,
        (None, false) => manifest.members.clone(),
    };
    let excluded: std::collections::BTreeSet<String> = manifest
        .exclude
        .iter()
        .filter_map(|e| member_path(e))
        .collect();
    let mut counted = std::collections::BTreeSet::new();
    for entry in listed {
        let Some(entry) = member_path(&entry) else {
            continue;
        };
        for folder in expand_member(&root, &entry)? {
            if !excluded.contains(&folder) && has_file(&root, &format!("{folder}/Cargo.toml")) {
                counted.insert(folder);
            }
        }
    }
    u32::try_from(counted.len()).ok().filter(|n| *n >= 2)
}

/// The member count to write on the detected `cargo test` step of `steps`: `Some` when the plan holds that step and the tree at `root` is a
/// workspace of more than one member ([`cargo_workspace_members`]).
pub fn detected_workspace_members(steps: &[PlannedStep], root: &Path) -> Option<u32> {
    steps
        .iter()
        .any(|s| {
            s.role == TestStepRole::Test
                && s.source == TestCommandSource::Detected
                && s.command == PLAN_NOTE_COMMAND
        })
        .then(|| cargo_workspace_members(root))
        .flatten()
}
