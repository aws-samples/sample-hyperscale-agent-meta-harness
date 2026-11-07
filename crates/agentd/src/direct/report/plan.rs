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
//!    script, `go.mod`, `Cargo.toml`, `pom.xml`, `build.gradle` with its wrapper).
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
    LoggedRun, MAX_REPORT_COMMAND_CHARS, MAX_REPORT_STEPS, PLAN_NOTE_COMMAND, TestCommandSource,
    TestStepRole,
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
    let mentions = |rel: &str, word: &str| read_source(root, rel).is_some_and(|t| t.contains(word));
    let python = has_file(root, "pytest.ini")
        || has_file(root, "conftest.py")
        || mentions("tox.ini", "pytest")
        || mentions("setup.cfg", "[tool:pytest]")
        || mentions("pyproject.toml", "pytest")
        || mentions("requirements.txt", "pytest")
        || mentions("requirements-dev.txt", "pytest");
    if python {
        if has_file(root, "requirements.txt") {
            steps.push(step(
                Setup,
                Detected,
                "python3 -m venv --system-site-packages .ham-venv && .ham-venv/bin/python -m pip install -q -r requirements.txt",
            ));
            steps.push(step(Test, Detected, ".ham-venv/bin/python -m pytest -q"));
        } else {
            steps.push(step(Test, Detected, "python3 -m pytest -q"));
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

/// The plan for the tree at `tree`.
pub fn plan(tree: &Path) -> Plan {
    let Ok(root) = tree.canonicalize() else {
        return Plan::NoCommand("the clean checkout cannot be read".into());
    };
    let records = Records::read(&root);
    for steps in [
        from_command_file(&root),
        entry_scripts(&root, &records),
        from_log(&root, &records),
        from_goal(&root, &records),
        detect(&root),
    ] {
        if steps.iter().any(|s| s.role == TestStepRole::Test) {
            return Plan::Steps(steps);
        }
    }
    Plan::NoCommand("the tree names no test command and has no test manifest".into())
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
