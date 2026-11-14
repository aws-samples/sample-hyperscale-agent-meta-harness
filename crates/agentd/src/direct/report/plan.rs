//! Which commands the clean-checkout report runs.
//!
//! The report runs the project's own test commands, never a command of the harness's choosing for a project that names none. Sources, in
//! this order, the first that gives a test command wins:
//!
//! 1. `.ham/test-command`, the project's own statement (one command per line; `#` comments; `setup: <command>` marks an install step);
//! 2. the lifecycle run log `.ham/lifecycle/LOG.md`: the test runs the engine logged, the last run of each command (a part of a suite is left
//!    out), so the report can set what the log said beside what a clean checkout gives. The notes a model writes after a command in the
//!    command column (`(3 tests)`, `# all green`, `exit 0`) are cut first (`command::strip_notes`), then a leading `timeout N`, the model's own
//!    limit on a run, then words of plain English written after the command without parentheses.
//!    A row whose note starts with the place of the run, the base commit
//!    (`(at BASE)`, `at BASE worktree`), was made outside this checkout and is left out; a note that only mentions the base
//!    commit belongs to a run of the head. A note in parentheses is cut from any command; words written after the command without parentheses are read
//!    only after a command of plain words, so `cd api && pytest -q at BASE worktree` and `pytest -q | tail -5 at BASE worktree` run as written;
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
//! A row of the log that names a part of a suite (a test file, a test id, a name filter: [`is_focused`]) is never a command of the plan: a part
//! must not stand for the suite. When such rows are the only ones of their language,
//! a plan made from the log or the goal card
//! that holds no whole-suite command of that language gets what source 4 gives for it, behind the model's own commands, and a plan that is
//! source 4 holds that step already: the step runs in place of the rows ([`PlanNotes::stand_ins`]). Source 4 reads the Python test files
//! at the root and directly in `tests/` and `test/`; pytest collects more, so for rows that ran pytest in a tree where source 4 finds no
//! Python command the step is `python3 -m pytest -q` when the tree holds test files in a folder below `tests/` or `test/` or at a path the
//! rows name ([`pytest_from_the_root`]), also when no source gave a command at all. Rows with nothing in their place (no
//! layout the report knows, a command that works in another folder, a file that names packages the report does not install: no Python step
//! is added to such a tree, with or without pytest settings, because it would fail for want of an install that is the report's own) are
//! [`PlanNotes::left_out`] with the reason, and a tree with no command at all names them in its reason. Nothing is added and nothing is said beside a
//! whole-suite command of the same language, beside a command that may run the suite of any language (`make test`, a script, tox), or for a plan
//! made from `.ham/test-command` or the repository's test scripts.
//!
//! The install steps. A test command needs the packages its tree declares, and a clean checkout has none of them. A plan made from the manifests
//! (source 4) carries the install step it knows: the lockfile or `npm install` for a `package.json`, and for a Python tree with a
//! `requirements.txt` the report's own virtual environment ([`VENV_INSTALL`]), in which the tests then run. Test commands that come from the
//! lifecycle log or the goal card (sources 2 and 3) are the model's own lines and carry no install, so the plan puts the same install steps in
//! front of them, each right before the first command it serves; the Node install also stands before a command that
//! may use the packages without naming a Node tool (`make test`). An install the report adds is the
//! [`Install`] of the commands that need it: when it fails, only those commands are not run, and a logged Python command that was moved into the
//! virtual environment runs as the log wrote it ([`PlannedStep::fallback`]). A plain Python layout whose packages are named by a file the report has no install step for
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
    LeftOutWhy, LoggedRun, MAX_PLAN_LINE_COMMAND_CHARS, MAX_REPORT_COMMAND_CHARS, MAX_REPORT_STEPS,
    NO_COMMAND_FOUND_TESTS, PLAN_NOTE_COMMAND, TestCommandSource, TestStepRole,
};
use std::io::Read;
use std::path::Path;

/// An install step the report adds for the test commands that need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// The Node packages at the root of the tree: `npm ci`, `npm install` or `pnpm install --frozen-lockfile`.
    Node,
    /// The report's virtual environment with the packages of `requirements.txt` ([`VENV_INSTALL`]).
    Venv,
}

/// One command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedStep {
    pub role: TestStepRole,
    pub source: TestCommandSource,
    pub command: String,
    /// For a command of the lifecycle log: what the log said about its last run.
    pub logged: Option<LoggedRun>,
    /// On a setup step: the install it is. When it fails, only the test steps that need the same install are not run; a setup step with
    /// `None` (a `setup:` line of `.ham/test-command`) stops every later step. On a test step: the install it needs.
    pub install: Option<Install>,
    /// On a test step of the log or the goal card that runs in the report's virtual environment: the command as it was written, which runs
    /// in its place when the environment could not be made.
    pub fallback: Option<String>,
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

/// The words of a pytest run that pytest reads as paths: for a command of plain words that starts `pytest` or `py.test`, or a Python
/// (`python`, `python3`, `python3.N`, the report's environment) with `-m pytest`, the words after it that are not options. The value of an
/// option is among them; the caller compares them with the word pytest said it did not find. `None` for any other command.
pub fn pytest_words(command: &str) -> Option<Vec<String>> {
    let Ok(Parsed::Plain { argv, .. }) = classify(command) else {
        return None;
    };
    let (program, rest) = argv.split_first()?;
    let name = base(program);
    let args = if name == "pytest" || name == "py.test" {
        rest
    } else if name.starts_with("python")
        && rest.first().is_some_and(|w| w == "-m")
        && rest.get(1).is_some_and(|w| w == "pytest")
    {
        &rest[2..]
    } else {
        return None;
    };
    Some(
        args.iter()
            .filter(|w| !w.starts_with('-'))
            .cloned()
            .collect(),
    )
}

/// Words of plain English that start a note a model wrote after a command without parentheses: `pytest -q at BASE worktree`,
/// `go test./... with the race detector`. None is a test runner's subcommand.
const PROSE_WORDS: [&str; 30] = [
    "after", "again", "against", "and", "at", "because", "before", "during", "for", "from", "in",
    "inside", "of", "on", "outside", "over", "per", "since", "then", "to", "under", "until",
    "using", "versus", "via", "vs", "when", "while", "with", "without",
];

/// `command` without the words of plain English a model wrote after it, and those words. The words start with one of [`PROSE_WORDS`],
/// in any letter case, that names nothing in the tree; no option (`-x`) follows it; and there are two words or more, or the one word does not
/// follow an option (one word after an option may be its value: `pytest -p at`). Only a command of plain words is read, so a quoted value is
/// one word, and the program itself is never cut. The `NAME=value` prefixes stay. Any other command is returned as it is, with no words.
fn without_prose(root: &Path, command: &str) -> (String, String) {
    let unchanged = || (command.to_string(), String::new());
    let Ok(Parsed::Plain { env, argv }) = classify(command) else {
        return unchanged();
    };
    let Some(start) = (1..argv.len()).find(|&i| {
        let tail = &argv[i..];
        PROSE_WORDS.contains(&argv[i].to_ascii_lowercase().as_str())
            && !root.join(&argv[i]).exists()
            && !tail.iter().any(|w| w.starts_with('-'))
            && (tail.len() >= 2 || !argv[i - 1].starts_with('-'))
    }) else {
        return unchanged();
    };
    let mut words: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", shell_word(v)))
        .collect();
    words.extend(argv[..start].iter().map(|w| shell_word(w)));
    (words.join(" "), argv[start..].join(" "))
}

/// The nouns after `base` that name a copy of the tree: `base worktree`, `base-commit checkout`.
const BASE_NOUNS: [&str; 6] = [
    "commit",
    "worktree",
    "worktrees",
    "checkout",
    "tree",
    "copy",
];

/// Whether `word` is a commit id as a model writes it after `base`: 7 to 40 hexadecimal digits.
fn is_commit_id(word: &str) -> bool {
    (7..=40).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A word that may stand in front of `base` in a note that places a run: `the base worktree`, `clean base-commit worktree`.
fn is_base_filler(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "the" | "a" | "clean" | "fresh" | "git"
    )
}

/// Whether one part of a note (its words) places the run at the base commit: `at`, `in` or `on`, the filler words, `base` in any letter case, and
/// then nothing, a noun of [`BASE_NOUNS`] or a commit id (`at BASE`, `in the base worktree`, `at base abc1234`); or `base` and a noun
/// (`base-commit checkout`, `clean base-commit worktree`); or the capital word `BASE` alone.
fn names_base_part(words: &[&str]) -> bool {
    let placed = words
        .first()
        .is_some_and(|w| matches!(w.to_ascii_lowercase().as_str(), "at" | "in" | "on"));
    let words = if placed { &words[1..] } else { words };
    let skip = words.iter().take_while(|w| is_base_filler(w)).count();
    let Some((base, after)) = words[skip..].split_first() else {
        return false;
    };
    if !base.eq_ignore_ascii_case("base") {
        return false;
    }
    match after.first() {
        None => placed || *base == "BASE",
        Some(next) => {
            BASE_NOUNS.contains(&next.to_ascii_lowercase().as_str())
                || (placed && is_commit_id(next))
        }
    }
}

/// Whether a note says the run was made at the base commit, in a copy of the tree other than this checkout. The contract asks for the note
/// `(at BASE)`, so only a note that starts with the place of the run counts: a note that merely speaks of the base commit (`(green; was red at base)`,
/// `(ran on base and head)`, `(BASE_URL unset)`, `(compare vs base: 3 failed)`, `on the base branch`) belongs to a run of the head, and dropping
/// that row can leave a whole suite out of the report. The note is read in parts, cut at `(`, `)`, `;`, `,`, `:` and `#`, and a part counts when
/// [`names_base_part`] says so; `(at BASE worktree)`, `at BASE worktree`, `(in the base worktree)`, `(base-commit checkout)` and `(clean base-commit
/// worktree)` are such notes, and `(base case)` and `(database tests)` are not. A note that also names the head (`at base: 2 failed, head: 0 failed`)
/// sets two runs side by side and is a row of the head. A word is made of letters, digits and `_`.
fn names_base(note: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if note
        .split(|c: char| !is_word(c))
        .any(|w| w.eq_ignore_ascii_case("head"))
    {
        return false;
    }
    note.split(['(', ')', ';', ',', ':', '#']).any(|part| {
        let words: Vec<&str> = part
            .split(|c: char| !is_word(c))
            .filter(|w| !w.is_empty())
            .collect();
        names_base_part(&words)
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
        install: None,
        fallback: None,
    }
}

/// `step` as a step of `install`: the install itself for a setup step, the install it needs for a test step.
fn of_install(install: Install, step: PlannedStep) -> PlannedStep {
    PlannedStep {
        install: Some(install),
        ..step
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

/// Whether `c` is a test command the plan may read: not empty, within the length of a command, no control character, a runner the report knows.
fn test_command(c: &str) -> bool {
    !c.is_empty()
        && c.chars().count() <= MAX_REPORT_COMMAND_CHARS
        && !c.chars().any(char::is_control)
        && is_test_command(c)
}

fn plain_command(c: &str) -> bool {
    test_command(c) && !is_focused(c)
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

/// A test command of the lifecycle log, made fit for the clean checkout, with what the log said of its last run.
struct LoggedRow {
    command: String,
    logged: LoggedRun,
    /// The command names a part of a suite ([`is_focused`]): it is no command of the plan.
    focused: bool,
}

/// Every plain test command of the lifecycle log made fit for the clean checkout, in the order of each command's last run.
fn logged_commands(root: &Path, records: &Records) -> Vec<(String, LoggedRun)> {
    logged_rows(root, records)
        .into_iter()
        .filter(|r| !r.focused)
        .map(|r| (r.command, r.logged))
        .collect()
}

/// Every test command of the lifecycle log made fit for the clean checkout, in the order of each command's last run: the whole-suite commands
/// and, marked, the ones that name a part of a suite. The two kinds never share a command, so each list is what it would be alone.
fn logged_rows(root: &Path, records: &Records) -> Vec<LoggedRow> {
    let Some(text) = records.log().and_then(|rel| read_source(root, &rel)) else {
        return Vec::new();
    };
    let mut seen: Vec<LoggedRow> = Vec::new();
    for line in text.lines() {
        let Some((column, logged)) = parse_log_line(line) else {
            continue;
        };
        // The log has the exit code and the counts in columns of their own, so text after the command in this column is a note. A
        // `timeout N` in front is the model's own limit on the run and not part of the command: the report gives every step its limit.
        // Words of plain English after the command are a note written without parentheses.
        let column = column.trim();
        let without_notes = strip_notes(column);
        let notes = column.get(without_notes.len()..).unwrap_or_default();
        let (command, prose) = without_prose(root, &strip_timeout(&without_notes));
        // A run the note places at the base commit was made outside this checkout: the report has nothing to set beside it.
        if names_base(notes) || names_base(&prose) {
            continue;
        }
        if !test_command(&command) {
            continue;
        }
        let focused = is_focused(&command);
        let Some(command) = fit_for_checkout(root, &command) else {
            continue;
        };
        seen.retain(|r| r.command != command);
        seen.push(LoggedRow {
            command,
            logged,
            focused,
        });
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

fn from_log(rows: &[LoggedRow]) -> Vec<PlannedStep> {
    let mut seen: Vec<&LoggedRow> = rows.iter().filter(|r| !r.focused).collect();
    let keep = seen.len().saturating_sub(MAX_TEST_STEPS);
    seen.drain(..keep);
    seen.into_iter()
        .map(|row| PlannedStep {
            logged: Some(row.logged),
            ..step(
                TestStepRole::Test,
                TestCommandSource::LifecycleLog,
                &row.command,
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
    steps.extend(python_steps(root, &commands));
    if let Some(pkg) = read_source(root, "package.json")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg)
        && let Some(test) = v.pointer("/scripts/test").and_then(|t| t.as_str())
        && !test.contains("no test specified")
    {
        let (install, test) = if has_file(root, "pnpm-lock.yaml") {
            ("pnpm install --frozen-lockfile", "pnpm test")
        } else if has_file(root, "package-lock.json") {
            ("npm ci", "npm test")
        } else {
            ("npm install", "npm test")
        };
        steps.push(of_install(Install::Node, step(Setup, Detected, install)));
        steps.push(of_install(Install::Node, step(Test, Detected, test)));
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

/// The steps of the Python test commands `commands` (the arguments that follow the interpreter) with the install step their tree needs:
/// the packages of `requirements.txt`, in the report's own virtual environment, in which the tests then run. Empty for no command.
fn python_steps(root: &Path, commands: &[String]) -> Vec<PlannedStep> {
    use TestCommandSource::Detected;
    use TestStepRole::{Setup, Test};
    let mut steps = Vec::new();
    if commands.is_empty() {
        return steps;
    }
    let venv = has_file(root, "requirements.txt");
    if venv {
        steps.push(of_install(
            Install::Venv,
            step(Setup, Detected, VENV_INSTALL),
        ));
    }
    for args in commands {
        steps.push(if venv {
            of_install(
                Install::Venv,
                step(Test, Detected, &format!("{VENV_PYTHON} {args}")),
            )
        } else {
            step(Test, Detected, &format!("python3 {args}"))
        });
    }
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

/// The most folders [`has_nested_python_tests`] reads.
const MAX_NESTED_DIRS: usize = 256;
/// How many folders below `tests/` or `test/` [`has_nested_python_tests`] goes down.
const MAX_NESTED_DEPTH: usize = 6;

/// Whether the tree holds a Python test file (`test_*.py`, `*_test.py`) in `tests/` or `test/` or in a folder below them, as far as the
/// reading goes. pytest collects such a file from the root; [`plain_python_test_files`] reads only the files directly in the two
/// folders. A link is not followed, and hidden folders, `node_modules` and `__pycache__` are passed over.
fn has_nested_python_tests(root: &Path) -> bool {
    let inside = |dir: &Path| {
        dir.canonicalize()
            .is_ok_and(|p| p.starts_with(root) && p.is_dir())
    };
    let mut folders: Vec<(std::path::PathBuf, usize)> = PLAIN_TEST_DIRS
        .iter()
        .map(|d| (root.join(d), 0))
        .filter(|(dir, _)| inside(dir))
        .collect();
    let mut read = 0;
    while let Some((dir, depth)) = folders.pop() {
        if read == MAX_NESTED_DIRS {
            return false;
        }
        read += 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok).take(10_000) {
            let (Ok(name), Ok(kind)) = (entry.file_name().into_string(), entry.file_type()) else {
                continue;
            };
            if kind.is_file() && python_test_name(&name) {
                return true;
            }
            if kind.is_dir()
                && depth < MAX_NESTED_DEPTH
                && !name.starts_with('.')
                && name != "node_modules"
                && name != "__pycache__"
            {
                folders.push((dir.join(&name), depth + 1));
            }
        }
    }
    false
}

/// The Python files `command` names: the words behind the program of each of its segments that end in `.py`, a test id cut off
/// (`tests/test_a.py::test_x` names `tests/test_a.py`), without a leading `./`. An option is no file.
fn named_python_files(command: &str) -> Vec<String> {
    first_words(command)
        .iter()
        .flat_map(|words| words.iter().skip(1).copied())
        .filter(|word| !word.starts_with('-'))
        .map(|word| word.split("::").next().unwrap_or(word))
        .filter(|path| path.ends_with(".py"))
        .map(|path| path.trim_start_matches("./").to_string())
        .collect()
}

/// What pytest gives a tree that the detection has no Python command for, when rows of the log that work at the root ran pytest on parts of
/// a suite: `python3 -m pytest -q` from the root, with the install its tree needs, when the tree holds test files that pytest
/// collects from there: in a folder below `tests/` or `test/` ([`has_nested_python_tests`]), or at a path one of the rows names (a file of
/// the tree with a name pytest collects, such as `src/textstats/test_textstats.py`). Empty for rows of another runner (a unittest row names
/// its files itself), for rows the shell has to read, and for a tree with no such file: a file whose name pytest does not collect runs only
/// when it is named, and that run would be a part of a suite.
fn pytest_from_the_root(root: &Path, at_root: &[(usize, &LoggedRow)]) -> Vec<PlannedStep> {
    let pytest_rows: Vec<&str> = at_root
        .iter()
        .map(|(_, row)| row.command.as_str())
        .filter(|command| pytest_words(command).is_some())
        .collect();
    if pytest_rows.is_empty() {
        return Vec::new();
    }
    let named = pytest_rows
        .iter()
        .flat_map(|command| named_python_files(command))
        .any(|file| python_test_name(base(&file)) && has_file(root, &file));
    if named || has_nested_python_tests(root) {
        python_steps(root, &["-m pytest -q".to_string()])
    } else {
        Vec::new()
    }
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

/// Whether every program `command` starts is one that does not use the Node packages of the tree: a runner of another language, and the `cd` and
/// the pipe consumers (`tee`, `tail`, `head`, `grep`, `cat`) that stand beside it. A command that is anything else (`make test`, a script, tox, a tool the
/// report does not know) may call `npm` or `jest` itself, so it is not known to need no install.
fn needs_no_node(command: &str) -> bool {
    first_words(command).iter().all(|w| {
        let program = base(w[0]);
        matches!(
            program,
            "go" | "cargo"
                | "python"
                | "python3"
                | "pytest"
                | "py.test"
                | "mvn"
                | "mvnw"
                | "gradle"
                | "gradlew"
                | "dotnet"
                | "cd"
                | "pushd"
                | "tee"
                | "tail"
                | "head"
                | "grep"
                | "cat"
        ) || program
            .strip_prefix("python3.")
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The steps of test commands that came from the lifecycle log or the goal card, with the install steps their tree needs. The
/// commands are the model's own lines and carry no install, and a clean checkout has no `node_modules` and none of the packages the session
/// installed: `npm test` then fails to load a test file of code whose tests pass. The install steps are the ones the detection knows:
///
/// - a command that starts a Node tool at the root gets the install of the lockfile, or of the packages `package.json` names ([`node_install`]). The
///   install stands before the first command that starts such a tool or that may use the packages without naming a tool (`make test`, a script, tox:
///   anything [`needs_no_node`] does not know), and after the commands of other languages that come first, so that a Go suite listed first runs first.
///   Only the commands that start a Node tool are stopped when it fails;
/// - a plain Python command at the root of a tree with a `requirements.txt` runs in the report's virtual environment ([`in_venv`]), which
///   [`VENV_INSTALL`] makes right before the first such command; the command as it was written is its [`PlannedStep::fallback`].
///
/// A command that changes the folder, a command of another runner and a Python command the shell has to read stay as they are and bring no
/// install. An install step that fails stops only the commands that need it (`report::run_steps`): they are listed as not run, and a Python
/// command runs as it was written.
fn with_installs(root: &Path, steps: Vec<PlannedStep>) -> Vec<PlannedStep> {
    use TestCommandSource::Detected;
    use TestStepRole::Setup;
    let needs_node = |s: &PlannedStep| works_at_root(&s.command) && runs_node(&s.command);
    // The Node install is added only when some command needs it.
    let node = node_install(root).filter(|_| steps.iter().any(needs_node));
    let venv = has_file(root, "requirements.txt");
    let (mut node_placed, mut venv_placed) = (false, false);
    let mut out = Vec::new();
    for mut s in steps {
        let serves = node.is_some() && needs_node(&s);
        if let Some(install) = node
            && !node_placed
            && (serves || !needs_no_node(&s.command))
        {
            out.push(of_install(Install::Node, step(Setup, Detected, install)));
            node_placed = true;
        }
        if serves {
            s.install = Some(Install::Node);
        } else if venv && let Some(command) = in_venv(&s.command) {
            if !venv_placed {
                out.push(of_install(
                    Install::Venv,
                    step(Setup, Detected, VENV_INSTALL),
                ));
                venv_placed = true;
            }
            s.fallback = Some(std::mem::replace(&mut s.command, command));
            s.install = Some(Install::Venv);
        }
        out.push(s);
    }
    out
}

// ---- the rows of the log that name a part of a suite --------------------------------------------------------------------------

/// Test commands of the lifecycle log that the plan left out because each names a part of a suite: the last one run and the number
/// of the others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOut {
    pub command: String,
    pub more: usize,
}

/// Test commands of the lifecycle log that were left out with nothing in their place, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Omitted {
    pub rows: LeftOut,
    pub why: LeftOutWhy,
    /// What the log said of the last run of the command `rows` names, when that run did not pass (an exit code other than 0, or failed
    /// tests). The command is not run in the clean checkout, so the report passes the log's word on.
    pub logged: Option<LoggedRun>,
}

/// What the plan says about the test commands of the lifecycle log that it left out. The report writes it at the head of the output
/// of the steps that carry it (`report::note_left_out`), and the summary reads it there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanNotes {
    /// A test step of the plan that the harness chose from the layout of the tree, by its place in the plan, and the commands of the log that
    /// it runs in place of: the rows of its language, which work at the root as it does.
    pub stand_ins: Vec<(usize, LeftOut)>,
    /// The commands of the log that were left out with nothing in their place: one entry for each reason, in the order of the first row
    /// that has it.
    pub left_out: Vec<Omitted>,
    /// The place of the first step the report added behind the commands of a plan made from the log or the goal card; every step from
    /// there on is its own addition. `None` for a plan it added nothing to and for a plan that holds no command of the model's. Such a
    /// step never makes the report read as timed out (`report::run_planned`): the model's commands have run, and their result stands.
    pub chosen_from: Option<usize>,
}

/// The language of a test command, by the program it starts. The detection has a layout for the first six. `Dotnet`, `Ruby` and
/// `Php` are the runners of one language that it has none for: such a command runs no suite of another language, so the rows of the other
/// languages keep their step, and a row of these languages that names a part of a suite is listed as left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Python,
    Node,
    Go,
    Rust,
    Maven,
    Gradle,
    Dotnet,
    Ruby,
    Php,
}

/// The language of the program that `words`, the words of one segment of a command, start. `None` for a program that may run the suite
/// of any language (`make`, a script, tox, which runs whatever its environments name) and for a segment that starts no runner (`cd api`).
fn family_of(words: &[&str]) -> Option<Family> {
    match base(words[0]) {
        "pytest" | "py.test" => Some(Family::Python),
        "npm" | "pnpm" | "yarn" | "bun" | "npx" | "pnpx" | "node" | "jest" | "vitest" | "mocha" => {
            Some(Family::Node)
        }
        "go" | "gotestsum" => Some(Family::Go),
        "cargo" => Some(Family::Rust),
        "mvn" | "mvnw" => Some(Family::Maven),
        "gradle" | "gradlew" => Some(Family::Gradle),
        "dotnet" => Some(Family::Dotnet),
        "rspec" => Some(Family::Ruby),
        "bundle" if words.get(1) == Some(&"exec") && words.get(2) == Some(&"rspec") => {
            Some(Family::Ruby)
        }
        "phpunit" => Some(Family::Php),
        // `python -m tox` is tox.
        program if program.starts_with("python") && !words.contains(&"tox") => Some(Family::Python),
        _ => None,
    }
}

/// The language of `command`: that of the first of its segments that starts a runner of one language (`cd api && pytest -q` is Python).
/// `None` for a command whose suite the report cannot tell from its program: `make test`, a script, tox.
fn family(command: &str) -> Option<Family> {
    first_words(command).iter().find_map(|w| family_of(w))
}

/// Every language `command` runs a suite of: the language of each of its segments that starts a runner of one language, once each
/// (`python3 -m pytest -q && npm test` runs the Python suite and the Node suite).
fn families(command: &str) -> Vec<Family> {
    let mut all = Vec::new();
    for words in first_words(command) {
        if let Some(language) = family_of(&words)
            && !all.contains(&language)
        {
            all.push(language);
        }
    }
    all
}

/// What the detection gives for one language: its test steps among `detected` and, in front of them, the install steps they need that
/// `have` does not hold yet.
fn steps_of_family(
    detected: &[PlannedStep],
    language: Family,
    have: &[PlannedStep],
) -> Vec<PlannedStep> {
    let tests: Vec<&PlannedStep> = detected
        .iter()
        .filter(|s| s.role == TestStepRole::Test && family(&s.command) == Some(language))
        .collect();
    let is_install = |s: &PlannedStep| s.role == TestStepRole::Setup && s.install.is_some();
    let mut out: Vec<PlannedStep> = detected
        .iter()
        .filter(|s| {
            is_install(s)
                && tests.iter().any(|t| t.install == s.install)
                && !have.iter().any(|h| is_install(h) && h.install == s.install)
        })
        .cloned()
        .collect();
    out.extend(tests.into_iter().cloned());
    out
}

/// `rows`, rows of the log with their place in it, as a [`LeftOut`]: the last one run and the number of the others.
fn left_out_of(rows: &[(usize, &LoggedRow)]) -> Option<LeftOut> {
    let (_, last) = rows.last()?;
    Some(LeftOut {
        command: last.command.clone(),
        more: rows.len() - 1,
    })
}

/// `steps`, the plan of a source, with a step of the layout for each language whose only rows in the lifecycle log name a part of a suite, and
/// what the plan says of the rows it left out. `rows` are the test commands of the log; `typed_by_the_model` says that the plan was
/// made from the log or the goal card.
///
/// For each language of such rows:
///
/// - a whole-suite command of the model's own for that language (a row of the log, a check of the goal card) runs the parts: nothing is
///   added and nothing is said. A command that chains the suites of two languages covers both ([`families`]);
/// - else the step the detection gives for the language runs in place of the rows that work at the root, where the detection looks. A plan
///   that is the detection holds the step already. A plan of the model's own lines gets it, with the install it needs, behind the model's
///   commands, so that those run first and as they did before;
/// - pytest rows of a tree where the detection has no Python command get `python3 -m pytest -q` when pytest collects test files from the
///   root that the detection does not read ([`pytest_from_the_root`]);
/// - a step stands in place of a Python row only when it runs the file the row names ([`reaches`]): a unittest discovery finds the files of
///   the folders it starts in, and pytest the files it collects. A row whose file no step runs is left out, and a step that would run none of
///   the rows is not added;
/// - no Python step is added at the root when the rows work in or name more than one project folder ([`project_folders`]): the engine ran
///   each project alone, and one pytest run from the root imports two test files of one name in two folders as one module and stops;
/// - rows with no such step (no layout the report knows, a row that works in another folder) were left out with nothing in their place;
/// - a Python step is never added to a tree whose packages are named by a file the report has no install step for
///   ([`uninstalled_python_manifest`]), whether the tree has pytest settings or not: run without those packages the suite would fail, and the
///   failure would be the report's own. The rows are left out, and the reason names the file.
///
/// A test step whose language the report cannot tell (`make test`, a script, tox) may run any suite, so a plan or a log that holds one is
/// left as it is, with no note: a second run of a suite the model ran through its own entry point could fail for want of what that entry
/// point sets up. A runner of one language the detection has no layout for (`dotnet test`, `rspec`, `phpunit`) runs no other suite: beside
/// it the rows of the other languages get their step or their line.
fn with_stand_ins(
    root: &Path,
    mut steps: Vec<PlannedStep>,
    typed_by_the_model: bool,
    rows: &[LoggedRow],
) -> (Vec<PlannedStep>, PlanNotes) {
    let mut notes = PlanNotes::default();
    let is_test = |s: &PlannedStep| s.role == TestStepRole::Test;
    // The rows that name a part of a suite, by language, each with its place in the log.
    let mut by_family: Vec<(Family, Vec<(usize, &LoggedRow)>)> = Vec::new();
    for (at, row) in rows.iter().enumerate().filter(|(_, r)| r.focused) {
        let Some(language) = family(&row.command) else {
            continue;
        };
        match by_family.iter_mut().find(|(known, _)| *known == language) {
            Some((_, list)) => list.push((at, row)),
            None => by_family.push((language, vec![(at, row)])),
        }
    }
    let untold = |command: &str| family(command).is_none();
    if by_family.is_empty()
        || steps.iter().any(|s| is_test(s) && untold(&s.command))
        || rows.iter().any(|r| !r.focused && untold(&r.command))
    {
        return (steps, notes);
    }
    let detected = if typed_by_the_model {
        detect(root)
    } else {
        Vec::new()
    };
    // The steps of the model's own lines; what stands behind them at the end is the report's addition.
    let own = steps.len();
    // The rows with nothing in their place, each with its place in the log and the reason.
    let mut uncovered: Vec<(usize, &LoggedRow, LeftOutWhy)> = Vec::new();
    for (language, list) in by_family {
        // A command covers the language when any of its segments starts a runner of it: `pytest -q && npm test` ran the Node suite too.
        let runs = |command: &str| families(command).contains(&language);
        let of_language = |s: &PlannedStep| is_test(s) && runs(&s.command);
        let own = |s: &PlannedStep| of_language(s) && s.source != TestCommandSource::Detected;
        if steps.iter().any(own) || rows.iter().any(|r| !r.focused && runs(&r.command)) {
            continue;
        }
        // A Python step the report adds would run without the packages of a tree whose packages a file it does not install names. The
        // detection refuses a plain layout for that reason; pytest settings in the tree change nothing about the missing install.
        if typed_by_the_model
            && language == Family::Python
            && let Some(file) = uninstalled_python_manifest(root)
        {
            let why = LeftOutWhy::Packages(Some(file));
            uncovered.extend(list.iter().map(|(at, row)| (*at, *row, why.clone())));
            continue;
        }
        // The log ran the Python tests of more than one project folder, each alone. A run from the root is not what the engine ran.
        if typed_by_the_model && language == Family::Python && project_folders(&list).len() > 1 {
            uncovered.extend(
                list.iter()
                    .map(|(at, row)| (*at, *row, LeftOutWhy::Projects)),
            );
            continue;
        }
        let (at_root, elsewhere): (Vec<_>, Vec<_>) = list
            .iter()
            .copied()
            .partition(|(_, row)| works_at_root(&row.command));
        uncovered.extend(elsewhere.into_iter().map(no_suite));
        // The test steps of the language that the plan holds (a plan that is the detection), else the ones the report would add.
        let held: Vec<usize> = (0..steps.len())
            .filter(|at| of_language(&steps[*at]))
            .collect();
        let mut extra = Vec::new();
        if held.is_empty() && typed_by_the_model && !at_root.is_empty() {
            extra = steps_of_family(&detected, language, &steps);
            if language == Family::Python && !extra.iter().any(is_test) {
                extra = steps_of_family(&pytest_from_the_root(root, &at_root), language, &steps);
            }
            if steps.len() + extra.len() > MAX_REPORT_STEPS {
                extra.clear();
            }
        }
        // The test commands the report has for the language, each with its place in the plan once the additions stand behind it.
        let behind = steps.len();
        let suite: Vec<(usize, String)> = held
            .iter()
            .map(|at| (*at, steps[*at].command.clone()))
            .chain(
                extra
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| is_test(s))
                    .map(|(at, s)| (behind + at, s.command.clone())),
            )
            .collect();
        // A step stands in place of the rows it runs. The others have nothing in their place.
        let why = if suite.is_empty() {
            LeftOutWhy::NoSuiteCommand
        } else {
            LeftOutWhy::NotReached
        };
        let mut in_place: Vec<(usize, Vec<(usize, &LoggedRow)>)> = Vec::new();
        for (at, row) in at_root {
            match stand_in_for(root, language, &suite, &row.command) {
                Some(place) => match in_place.iter_mut().find(|(known, _)| *known == place) {
                    Some((_, rows)) => rows.push((at, row)),
                    None => in_place.push((place, vec![(at, row)])),
                },
                None => uncovered.push((at, row, why.clone())),
            }
        }
        if !in_place.is_empty() {
            // The additions; none when the plan held the steps already.
            steps.extend(extra);
            in_place.sort_by_key(|(place, _)| *place);
            notes.stand_ins.extend(
                in_place
                    .iter()
                    .filter_map(|(place, rows)| left_out_of(rows).map(|left| (*place, left))),
            );
        }
    }
    uncovered.sort_by_key(|(at, ..)| *at);
    notes.left_out = omitted_by_reason(&uncovered);
    notes.chosen_from = (typed_by_the_model && own > 0 && steps.len() > own).then_some(own);
    (steps, notes)
}

/// The place in the plan of the step that stands in place of `row`, a logged command that names a part of a suite. `suite` are
/// the test commands the report has for `language`, each with its place. `None` when none of them runs what the row names ([`reaches`]).
/// Else the first command that runs a file the row names, so the note is on the step that ran the logged tests (of two unittest
/// discoveries, the one that starts where the file is); the first command of the suite for a row that names no file and for a language
/// other than Python.
fn stand_in_for(
    root: &Path,
    language: Family,
    suite: &[(usize, String)],
    row: &str,
) -> Option<usize> {
    let commands: Vec<String> = suite.iter().map(|(_, command)| command.clone()).collect();
    if !reaches(root, language, &commands, row) {
        return None;
    }
    let files = if language == Family::Python {
        named_python_files(row)
    } else {
        Vec::new()
    };
    suite
        .iter()
        .find(|(_, command)| {
            files
                .iter()
                .any(|file| python_command_runs(root, command, file))
        })
        .or(suite.first())
        .map(|(place, _)| *place)
}

/// Whether one of `suite`, the test commands the report has for `language`, runs what `row` names, a logged command that names a part of
/// a suite. `false` for no command. For a language other than Python the command of the whole suite stands for every part. A Python
/// row is reached when one of the commands runs each file it names ([`python_command_runs`]); a row that names no file (a name filter) is a
/// part of whatever the suite collects.
fn reaches(root: &Path, language: Family, suite: &[String], row: &str) -> bool {
    !suite.is_empty()
        && (language != Family::Python
            || named_python_files(row).iter().all(|file| {
                suite
                    .iter()
                    .any(|command| python_command_runs(root, command, file))
            }))
}

/// Whether `command`, a Python test command of the report's own choosing, runs `file`, a file a logged command names. The file must
/// be in the tree, outside hidden folders. A unittest discovery (`-m unittest discover [-s <folder>] -v`) finds the files named `test*.py`
/// in the folder it starts in and in the packages below it ([`discovery_reaches`]). pytest from the root collects the files named
/// `test_*.py` and `*_test.py` unless the settings of the tree narrow that ([`pytest_collects`]).
fn python_command_runs(root: &Path, command: &str, file: &str) -> bool {
    if !has_file(root, file) || file.split('/').any(|part| part.starts_with('.')) {
        return false;
    }
    let name = base(file);
    let folder = file.rsplit_once('/').map_or("", |(folder, _)| folder);
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.contains(&"unittest") {
        let start = words
            .iter()
            .position(|w| *w == "-s")
            .and_then(|at| words.get(at + 1))
            .copied()
            .unwrap_or_default();
        return name.starts_with("test")
            && name.ends_with(".py")
            && discovery_reaches(root, start, folder);
    }
    python_test_name(name) && pytest_collects(root, file)
}

/// Whether a unittest discovery that starts in `start` (the root for an empty name) reads the files of `folder`: the folder it starts in,
/// or a folder below it when every folder on the way down is a package.
fn discovery_reaches(root: &Path, start: &str, folder: &str) -> bool {
    let below = if start.is_empty() {
        folder
    } else if folder == start {
        ""
    } else {
        match folder
            .strip_prefix(start)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(below) => below,
            None => return false,
        }
    };
    let mut at = start.to_string();
    below
        .split('/')
        .filter(|part| !part.is_empty())
        .all(|part| {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(part);
            has_file(root, &format!("{at}/__init__.py"))
        })
}

/// The files at the root of a tree that may hold pytest's settings, in the order pytest looks for them, each with the words that make
/// it the file pytest reads: `pytest.ini` whatever it holds, the others when they hold pytest's section.
const PYTEST_SETTINGS: [(&str, &str); 4] = [
    ("pytest.ini", ""),
    ("pyproject.toml", "[tool.pytest.ini_options]"),
    ("tox.ini", "[pytest]"),
    ("setup.cfg", "[tool:pytest]"),
];

/// The value of the setting `key` on `line` (`key = value`): the text behind the `=`. `None` for a line of another key.
fn setting_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.trim_start()
        .strip_prefix(key)?
        .trim_start()
        .strip_prefix('=')
}

/// Whether pytest, run from the root, collects `file`, a file of the tree with a name it collects by default. The settings of the
/// tree may narrow that. pytest reads one settings file, the first of [`PYTEST_SETTINGS`] that is its own. With `testpaths` in it pytest
/// collects only the files and the folders the setting names (the value is read over the lines it runs on), and with a `python_files`
/// pattern the reading cannot say, so the answer is no. A tree with neither setting collects the file.
fn pytest_collects(root: &Path, file: &str) -> bool {
    let Some(text) = PYTEST_SETTINGS
        .iter()
        .find_map(|(name, section)| read_source(root, name).filter(|text| text.contains(section)))
    else {
        return true;
    };
    let mut paths: Vec<String> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if setting_of(line, "python_files").is_some() {
            return false;
        }
        let Some(first) = setting_of(line, "testpaths") else {
            continue;
        };
        // The value may go on over indented lines (an ini file) or to the end of its brackets (a TOML array).
        let mut value = first.to_string();
        while lines.peek().is_some_and(|next| {
            next.starts_with([' ', '\t']) || (value.contains('[') && !value.contains(']'))
        }) {
            value.push(' ');
            value.push_str(lines.next().unwrap_or_default());
        }
        paths.extend(
            value
                .split(|c: char| c.is_whitespace() || "[],\"'".contains(c))
                .filter(|path| !path.is_empty())
                .map(|path| {
                    path.trim_start_matches("./")
                        .trim_end_matches('/')
                        .to_string()
                }),
        );
    }
    paths.is_empty()
        || paths.iter().any(|path| {
            path == "."
                || path.is_empty()
                || file
                    .strip_prefix(path.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
}

/// The project folders that `rows`, logged Python commands that name a part of a suite, work in, once each. A row that changes
/// the folder works in that folder ([`folder_of`]). A row at the root works in the project of each file it names ([`project_of`]), and in
/// the root (an empty name) when it names none.
fn project_folders(rows: &[(usize, &LoggedRow)]) -> Vec<String> {
    let mut folders: Vec<String> = Vec::new();
    let mut add = |folder: String| {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    };
    for (_, row) in rows {
        if !works_at_root(&row.command) {
            add(folder_of(&row.command));
            continue;
        }
        let files = named_python_files(&row.command);
        if files.is_empty() {
            add(String::new());
        }
        for file in files {
            add(project_of(&file));
        }
    }
    folders
}

/// The folder of the project a test file belongs to, as far as its path says: what stands before its `tests` or `test` folder (`api` for
/// `api/tests/test_models.py`), and the root (an empty name) for a path with no such folder.
fn project_of(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    parts
        .iter()
        .position(|part| PLAIN_TEST_DIRS.contains(part))
        .map(|at| parts[..at].join("/"))
        .unwrap_or_default()
}

/// The folder a command works in that does not work at the root ([`works_at_root`]): the folder its `cd` or `pushd` names, or the value of
/// its tool's folder option, without quotes, a leading `./` and a trailing `/`. The command itself when the folder cannot be read, so that
/// two such commands count as two folders.
fn folder_of(command: &str) -> String {
    let clean = |folder: &str| {
        folder
            .trim_matches(['\'', '"', ')'])
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    };
    for words in first_words(command) {
        let mut tokens = words
            .iter()
            .map(|t| t.trim_start_matches('('))
            .filter(|t| !t.is_empty());
        if matches!(tokens.next(), Some("cd" | "pushd"))
            && let Some(folder) = tokens.next()
        {
            return clean(folder);
        }
        for (at, word) in words.iter().enumerate() {
            if matches!(*word, "--prefix" | "-C" | "--cwd" | "--dir")
                && let Some(folder) = words.get(at + 1)
            {
                return clean(folder);
            }
            if let Some(folder) = ["--prefix=", "--cwd=", "--dir="]
                .iter()
                .find_map(|option| word.strip_prefix(option))
            {
                return clean(folder);
            }
        }
    }
    command.to_string()
}

/// A row of the log with nothing in its place because the report found no command for the whole suite.
fn no_suite((at, row): (usize, &LoggedRow)) -> (usize, &LoggedRow, LeftOutWhy) {
    (at, row, LeftOutWhy::NoSuiteCommand)
}

/// `uncovered`, rows of the log in the order they were run, each with the reason nothing stands in its place, as one [`Omitted`] for each
/// reason, in the order of the first row that has it.
fn omitted_by_reason(uncovered: &[(usize, &LoggedRow, LeftOutWhy)]) -> Vec<Omitted> {
    let mut reasons: Vec<&LeftOutWhy> = Vec::new();
    for (_, _, why) in uncovered {
        if !reasons.contains(&why) {
            reasons.push(why);
        }
    }
    reasons
        .into_iter()
        .filter_map(|why| {
            let group: Vec<(usize, &LoggedRow)> = uncovered
                .iter()
                .filter(|(_, _, w)| w == why)
                .map(|(at, row, _)| (*at, *row))
                .collect();
            let logged = group.last().map(|(_, row)| row.logged).filter(|run| {
                run.exit_code.is_some_and(|code| code != 0) || run.failed.is_some_and(|n| n > 0)
            });
            left_out_of(&group).map(|rows| Omitted {
                rows,
                why: why.clone(),
                logged,
            })
        })
        .collect()
}

/// `why`, the reason of a tree with no command, with the test commands of the lifecycle log that name a part of a suite: the log had
/// test runs, and the reason says why they gave no command. The last one run is named, cut as a line about the plan cuts it, and the others
/// are counted.
fn with_left_out(why: String, rows: &[LoggedRow]) -> String {
    let focused: Vec<&LoggedRow> = rows.iter().filter(|r| r.focused).collect();
    let Some(last) = focused.last() else {
        return why;
    };
    let command: String = last
        .command
        .chars()
        .take(MAX_PLAN_LINE_COMMAND_CHARS)
        .map(|c| if c == '`' { '\'' } else { c })
        .collect();
    match focused.len() - 1 {
        0 => format!(
            "{why}; `{command}` of the lifecycle log names a test file, a test or a filter and was left out"
        ),
        n => format!(
            "{why}; `{command}` and {n} more of the lifecycle log name test files, tests or filters and were left out"
        ),
    }
}

/// The plan for the tree at `tree`, and what it says of the test commands of the lifecycle log that it left out.
pub fn plan_with_notes(tree: &Path) -> (Plan, PlanNotes) {
    let Ok(root) = tree.canonicalize() else {
        return (
            Plan::NoCommand("the clean checkout cannot be read".into()),
            PlanNotes::default(),
        );
    };
    let records = Records::read(&root);
    let has_test = |steps: &[PlannedStep]| steps.iter().any(|s| s.role == TestStepRole::Test);
    // The project's own statement and its own scripts are run as they are: the log did not make that plan, so none of its rows was left out of it.
    for steps in [from_command_file(&root), entry_scripts(&root, &records)] {
        if has_test(&steps) {
            return (Plan::Steps(steps), PlanNotes::default());
        }
    }
    let rows = logged_rows(&root, &records);
    // Each source with whether its commands are the model's own lines, which carry no install step (the log and the goal card).
    for (steps, typed_by_the_model) in [
        (from_log(&rows), true),
        (from_goal(&root, &records), true),
        (detect(&root), false),
    ] {
        if has_test(&steps) {
            let steps = if typed_by_the_model {
                with_installs(&root, steps)
            } else {
                steps
            };
            let (steps, notes) = with_stand_ins(&root, steps, typed_by_the_model, &rows);
            return (Plan::Steps(steps), notes);
        }
    }
    // No source gave a command. Rows of the log that name a part of a suite may still have a suite the report can run in their place: a
    // Python project whose only test file sits beside the code has no layout the detection reads.
    let (steps, notes) = with_stand_ins(&root, Vec::new(), true, &rows);
    if has_test(&steps) {
        return (Plan::Steps(steps), notes);
    }
    let why = plain_layout_not_run(&root).unwrap_or_else(|| NO_COMMAND_REASON.into());
    (
        Plan::NoCommand(with_left_out(why, &rows)),
        PlanNotes::default(),
    )
}

/// The plan for the tree at `tree`.
pub fn plan(tree: &Path) -> Plan {
    plan_with_notes(tree).0
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
