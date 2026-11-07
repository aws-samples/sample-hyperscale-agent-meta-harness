//! The clean checkout and the commands run in it.
//!
//! `checkout` makes a fresh clone of the delivered bundle at the delivered head: no state of the engine's work tree reaches it (no
//! ignored file, no untracked file, no cache). `run_step` reads one command first: words become an argument
//! vector with no shell, shell syntax goes to `sh -c` after `sh -n` has read it to the end, and a command that cannot be read is not run. It
//! then runs the command as the unprivileged test user, in its own
//! process group, with a clean environment (a fresh `HOME` and `TMPDIR`, the package-source variables of the run and nothing else of the
//! engine's), no input, a time limit, and a bounded reader of both output streams ([`super::parse::OutputScan`]). When the step ends,
//! by itself or at its limit, its whole process group is stopped, so a background process cannot hold the report open. A process that
//! starts its own session leaves the group; the microVM that ends the run is the last boundary for it.
//!
//! Nothing a step prints is trusted: the counts come only from the scanned stream, never from a file the step wrote, and the command and
//! the tail are redacted before they are kept.

use crate::direct::report::command::{self, Parsed, UNREADABLE};
use crate::direct::report::parse::{OutputScan, last_bytes};
use crate::direct::report::plan::{PlannedStep, first_words, uses_container_tools};
use crate::engine::{LaunchSpec, command_argv};
use harness_core::refusal::redact;
use harness_protocol::direct::{
    NEEDS_PREFIX, NOT_RUN_MARK, NOT_RUN_PREFIX, PLAN_NOTE_COMMAND, PLAN_NOTE_PREFIX,
    STAND_IN_PREFIX, TestCommandSource, TestCounts, TestStep, TestStepRole, plan_note_line,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How long the pipes are read after a step's process group was stopped.
const DRAIN: Duration = Duration::from_millis(500);
/// How long the shell has to read a command to its end ([`shell_reads`]).
const READ_LIMIT: Duration = Duration::from_secs(10);

/// What every step of one report shares.
#[derive(Clone, Debug)]
pub struct RunEnv {
    /// A directory the test user cannot write: `home/` and `tmp/` of the steps are made under it.
    pub work: PathBuf,
    /// `(uid, gid)` of the test user; `None` runs the steps as agentd's own user.
    pub run_as: Option<(u32, u32)>,
    /// The Landlock sandbox the steps start in, with `work` as its writable folder; `None` in the `uid` mode.
    pub sandbox: Option<crate::sandbox::Sandbox>,
    /// Directories searched for programs; also the steps' `PATH`.
    pub path: Vec<PathBuf>,
    /// The engine's environment; only the package-source variables are passed on.
    pub engine_env: BTreeMap<String, String>,
    /// The engine's home directory: a Maven settings file there is copied.
    pub engine_home: Option<PathBuf>,
    /// The cargo `config.toml` the package proxy gave this run (crates.io replaced by the proxy's registry; no secret). The steps get a file
    /// with this text, never a file the engine wrote; `None` when the run had no proxy.
    pub cargo_config: Option<String>,
}

impl RunEnv {
    /// The session started a container runtime: the engine's environment names its endpoint.
    pub fn has_container_runtime(&self) -> bool {
        self.engine_env.contains_key("DOCKER_HOST")
    }
}

/// Why a step was not run when the session has no container runtime.
pub const NO_RUNTIME: &str = "the session has no container runtime";
/// Why a step is marked not run when the session started a runtime and the command still found none to talk to.
pub const RUNTIME_DOWN: &str = "the session's container runtime did not answer";
/// The start of the reason of a step that failed and printed the system's words for a full disk; the line follows after `: `.
pub const OUT_OF_DISK: &str = "ran out of disk";
/// The reason of a step that was not started because an earlier one filled the disk: on a volume that is still full its result would say nothing.
pub const SKIPPED_FOR_DISK: &str = "skipped: the session ran out of disk in an earlier command";

/// Whether a variable of the engine's environment reaches the steps. The report gets the environment of the engine's own session where it points
/// at services of the session: the package tools' settings (the loopback package proxy; `MAVEN_ARGS` is replaced by [`step_env`] with the step's
/// own copy of the settings file) and the four variables of the container runtime. Nothing that carries a credential or the engine's own
/// configuration: no `AWS_*`, no engine settings.
pub fn passes_to_steps(name: &str) -> bool {
    name.starts_with("PIP_")
        || name.starts_with("UV_")
        || name.starts_with("npm_config_")
        || name.starts_with("pnpm_config_")
        || name.starts_with("TESTCONTAINERS_")
        || matches!(
            name,
            "GOPROXY"
                | "GOSUMDB"
                | "GOFLAGS"
                | "GOTOOLCHAIN"
                | "MVNW_REPOURL"
                | "MAVEN_ARGS"
                // Cargo: the build switches. The steps' `CARGO_HOME` is theirs, made from the proxy's text in [`step_env`]; the engine's
                // own is never passed.
                | "CARGO_BUILD_JOBS"
                | "CARGO_PROFILE_DEV_DEBUG"
                | "CARGO_PROFILE_TEST_DEBUG"
                | "CARGO_INCREMENTAL"
                | "CARGO_NET_GIT_FETCH_WITH_CLI"
                | "RUST_BACKTRACE"
                | "CONTAINER_HOST"
                | "DOCKER_HOST"
                | "PYTHONDONTWRITEBYTECODE"
                // The variables of the local servers the report started itself: `report::services` puts them in the steps' environment,
                // and only the ones the repository's own files read by name.
                | "DATABASE_URL"
                | "BROKER_URL"
                | "BROKER_PASSWORD"
        )
}

fn is_head(head: &str) -> bool {
    head.len() == 40 && head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "safe.directory=*",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "advice.detachedHead=false",
    ])
    .args(args)
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_LFS_SKIP_SMUDGE", "1")
    .stdin(Stdio::null())
    .kill_on_drop(true);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first: String = err.trim().chars().take(200).collect();
        return Err(format!("git {}: {first}", args.first().unwrap_or(&"")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Clones `bundle` into `dest` (which must not exist) and checks out `head`, detached.
pub async fn checkout(bundle: &Path, head: &str, dest: &Path) -> Result<(), String> {
    if !is_head(head) {
        return Err("the delivered head is not a commit id".into());
    }
    let bundle_arg = bundle.display().to_string();
    let dest_arg = dest.display().to_string();
    git(
        None,
        &[
            "clone",
            "-q",
            "--no-checkout",
            "--no-hardlinks",
            &bundle_arg,
            &dest_arg,
        ],
    )
    .await?;
    git(Some(dest), &["checkout", "-q", "--detach", head]).await?;
    let now = git(Some(dest), &["rev-parse", "HEAD"]).await?;
    if now != head {
        return Err("the checkout is not at the delivered head".into());
    }
    Ok(())
}

/// Stops every process of the group `pid` leads.
async fn kill_group_of(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .status()
        .await;
}

fn step_env(env: &RunEnv, home: &Path, tmp: &Path) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = [
        ("HOME", home.display().to_string()),
        ("TMPDIR", tmp.display().to_string()),
        ("LANG", "C.UTF-8".to_string()),
        ("TERM", "dumb".to_string()),
        ("CI", "1".to_string()),
        // Every engine process gets this (`emit.rs`) and the worker image sets it: no credential comes from instance metadata
        // (invariant 2). Set here after the engine's variables are filtered, so nothing in the engine's environment turns it back on.
        ("AWS_EC2_METADATA_DISABLED", "true".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in &env.engine_env {
        if passes_to_steps(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    // In a sandbox the step runs as agentd's own user: its home and temp folder are named for the JVM and for Gradle (see `sandbox::tool_env`).
    if env.sandbox.is_some() {
        let names = crate::sandbox::tool_env(&out, home, tmp);
        out.extend(names);
    }
    // Maven: Java's `user.home` is the password database's, not `$HOME`, so a settings file in the step's home is not read. The engine's session
    // names its file through `MAVEN_ARGS` (Maven 3.9 reads it, the wrapper's Maven too); the step names its own copy the same way, and only when
    // the copy exists. The engine's value would name a file of the engine's home.
    let copy = home.join(".m2/settings.xml");
    if env.engine_env.contains_key("MAVEN_ARGS") && copy.is_file() {
        out.insert(
            "MAVEN_ARGS".to_string(),
            format!("--settings {}", copy.display()),
        );
    } else {
        out.remove("MAVEN_ARGS");
    }
    // Cargo: the step's own `.cargo/config.toml`, made from the text the package proxy gave the run (the source replacement, no secret), is
    // its `CARGO_HOME`. A file the engine wrote could name a compiler wrapper or a test runner, so the engine's is never read.
    if env.cargo_config.is_some() && home.join(".cargo/config.toml").is_file() {
        out.insert(
            "CARGO_HOME".to_string(),
            home.join(".cargo").display().to_string(),
        );
    } else {
        out.remove("CARGO_HOME");
    }
    out
}

/// Why `command` cannot run in this session, decided before it starts: it starts a container tool and the session has no container runtime.
fn not_run_before(env: &RunEnv, command: &str) -> Option<&'static str> {
    (!env.has_container_runtime() && uses_container_tools(command)).then_some(NO_RUNTIME)
}

/// The program a command starts: the first word of its first segment that is not a `cd`, after any leading `NAME=value` assignments. `cargo` for
/// `cargo test`, `CI=1 cargo test` and `cd api && cargo test`; the program of a later segment (`true && cargo test`) does not count, because
/// the earlier part ran.
fn leading_program(command: &str) -> Option<String> {
    first_words(command)
        .into_iter()
        .map(|w| w[0].to_string())
        .find(|program| program.as_str() != "cd")
}

/// Whether the shell's "not found" line explains the exit code of the step: the step exited 127, did not time out, and the program the output
/// says is missing is the program the command starts. A repository script that runs a missing tool inside (the command starts `sh`), a file of
/// the repository, and any other exit code are results of the repository's own commands and stay as they are.
fn program_missing_here<'a>(
    command: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    named: Option<&'a str>,
) -> Option<&'a str> {
    let program = named?;
    (!timed_out && exit_code == Some(127) && leading_program(command).as_deref() == Some(program))
        .then_some(program)
}

/// The line that explains a failed step as a full disk: the step ended with a code other than 0 (not at its limit), the output said the disk is full,
/// and the runner did not report failing tests of its own (counts that show a failure leave the step red; counts of tests that passed before the
/// disk filled go with the step, they measure an incomplete run). The repository's output can print the words; the line is shown with the step, so
/// a reader sees what was said, and a step that exited 0 is never changed.
fn disk_full_here(
    exit_code: Option<i32>,
    timed_out: bool,
    counts: Option<TestCounts>,
    line: Option<&str>,
) -> Option<&str> {
    let line = line?;
    (!timed_out && exit_code.is_some_and(|c| c != 0) && counts.is_none_or(|c| c.failed == 0))
        .then_some(line)
}

/// Whether `step` is the one that ran out of disk ([`OUT_OF_DISK`]): the later steps are skipped.
pub fn ran_out_of_disk(step: &TestStep) -> bool {
    step.is_not_run()
        && step
            .not_run_reason()
            .is_some_and(|r| r.starts_with(OUT_OF_DISK))
}

/// Puts the plan note on a `cargo test` step the harness chose from the manifest: with `Some(members)`, the line
/// [`plan_note_line`] at the head of the step's output, after the `NOT RUN:` line when agentd wrote one, so the reason stays the first line; the
/// output gives way to it from the front, within `cap` bytes. The runtime block of a step not run for want of a runtime stays whole and
/// goes behind the note, because the note is read from the line after the reason. A line that starts like the note and was printed by the
/// repository at that place is removed either way, so only agentd's line is ever read. Any other step is left as it is.
pub fn set_plan_note(step: &mut TestStep, members: Option<u32>, cap: usize) {
    if step.role != TestStepRole::Test
        || step.source != TestCommandSource::Detected
        || step.command != PLAN_NOTE_COMMAND
    {
        return;
    }
    let (skip, block_lines) = runtime_block_span(step);
    let tail = step.output_tail.clone();
    let (head, after_head) = if step.is_not_run() && tail.starts_with(NOT_RUN_PREFIX) {
        match tail.split_once('\n') {
            Some((first, rest)) => (format!("{first}\n"), rest),
            None => (format!("{tail}\n"), ""),
        }
    } else {
        (String::new(), tail.as_str())
    };
    // The head lines agentd wrote before the block, the block, and the output.
    let (before, after_before) = split_lines(after_head, skip);
    let (block, rest) = split_lines(after_before, block_lines);
    // A block that ends the tail has no newline after its last line; what is written behind it must not be glued to it.
    let mut block = block.to_string();
    if !block.is_empty() && !block.ends_with('\n') {
        block.push('\n');
    }
    let strip = |mut text: &'_ str| {
        while text.starts_with(PLAN_NOTE_PREFIX) {
            text = text.split_once('\n').map_or("", |(_, after)| after);
        }
        text.to_string()
    };
    let before_kept = strip(before);
    // Without a block the output follows the reason, and a note the repository printed there is removed. With one, the output starts behind the
    // block, where nothing reads a note.
    let kept = if block_lines == 0 {
        strip(rest)
    } else {
        rest.to_string()
    };
    let Some(members) = members else {
        if before_kept.len() != before.len() || kept.len() != rest.len() {
            step.output_tail = format!("{head}{before_kept}{block}{kept}");
        }
        return;
    };
    let line = format!("{}\n", plan_note_line(members));
    let fixed = head.len() + line.len() + before_kept.len();
    let shown = block_that_fits(&block, fixed, kept.len(), cap);
    let (output, cut) = last_bytes(&kept, cap.saturating_sub(fixed + shown.len()));
    step.output_tail = format!("{head}{line}{before_kept}{shown}{output}");
    step.output_truncated |= cut || shown.len() < block.len();
}

/// The first `n` lines of `s`, each with its newline, and the rest.
fn split_lines(s: &str, n: usize) -> (&str, &str) {
    let end = s.split_inclusive('\n').take(n).map(str::len).sum();
    s.split_at(end)
}

/// Whether `line` is one that agentd writes at the head of a step's output, after the reason: the plan note, a `NEEDS:` line, a stand-in sentence.
fn is_head_line(line: &str) -> bool {
    line.starts_with(PLAN_NOTE_PREFIX)
        || line.starts_with(NEEDS_PREFIX)
        || line.starts_with(STAND_IN_PREFIX)
}

/// Where the runtime block is in the output of `step`, as `(skip, lines)`: the number of head lines (the plan note and the like) between
/// the `NOT RUN:` line and the block, and the number of lines of the block, which are the lines behind [`RUNTIME_PREFIX`]. Only a step that was
/// not run for want of a container runtime ([`NO_RUNTIME`], [`RUNTIME_DOWN`]) has a block: a line of that shape in the output of any other step is
/// output, and `(0, 0)` says there is none.
pub fn runtime_block_span(step: &TestStep) -> (usize, usize) {
    if !step.is_not_run() || !matches!(step.not_run_reason(), Some(NO_RUNTIME | RUNTIME_DOWN)) {
        return (0, 0);
    }
    let after: Vec<&str> = step.output_tail.lines().skip(1).collect();
    let skip = after.iter().take_while(|l| is_head_line(l)).count();
    let lines = after[skip..]
        .iter()
        .take_while(|l| l.starts_with(RUNTIME_PREFIX))
        .count();
    if lines == 0 { (0, 0) } else { (skip, lines) }
}

/// The leading whole lines of `block` (each ends in a newline) that may stay when `other` more bytes of head are written beside them and the end
/// of the output, `end` bytes long, is cut from its front to fit `cap`: the end keeps `min(end, TAIL_FLOOR)` bytes, and the block gives up its last
/// lines to let it. A head line written before the block (the plan note, a `NEEDS:` line) makes the block lose lines, never a part of one.
pub fn block_that_fits(block: &str, other: usize, end: usize, cap: usize) -> &str {
    let floor = end.min(TAIL_FLOOR);
    let mut keep = block.len();
    while keep > 0 && cap.saturating_sub(other + keep) < floor {
        keep = block[..keep - 1].rfind('\n').map_or(0, |i| i + 1);
    }
    &block[..keep]
}

/// The first word of each line [`add_runtime_block`] puts after the `NOT RUN:` line.
pub const RUNTIME_PREFIX: &str = "RUNTIME: ";
/// The most bytes of the socket line of the runtime block.
pub const RUNTIME_SOCKET_LINE_BYTES: usize = 240;
/// How long the connect to the runtime's socket is given.
const SOCKET_PROBE: Duration = Duration::from_secs(2);
/// What the end of a step's output keeps at least, beside the reason and the runtime block, so that the block never takes the whole tail.
const TAIL_FLOOR: usize = 1024;

/// What the session's Docker socket does when agentd connects to it: `None` when the engine's environment names no `unix://` socket. The
/// connect ends at once or after two seconds at most, and sends nothing, so the front that listens there sees a connection that closes.
/// Run after a step that found no runtime, it tells a socket file that nobody listens behind (`connection refused`, what a stopped front leaves)
/// from a socket that answers and a runtime the repository's own configuration did not find.
pub async fn runtime_socket_state(engine_env: &BTreeMap<String, String>) -> Option<String> {
    let path = engine_env.get("DOCKER_HOST")?.strip_prefix("unix://")?;
    if path.is_empty() {
        return None;
    }
    let what = match tokio::time::timeout(SOCKET_PROBE, tokio::net::UnixStream::connect(path)).await
    {
        Err(_) => "did not accept a connection within 2 s".to_string(),
        Ok(Ok(_)) => "accepts connections".to_string(),
        Ok(Err(e)) => socket_error_words(&e),
    };
    let line = format!("socket {path}: {what}");
    Some(first_chars(&line, RUNTIME_SOCKET_LINE_BYTES))
}

/// How a failed connect to the runtime's socket reads in the runtime block.
pub fn socket_error_words(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "does not exist".to_string(),
        std::io::ErrorKind::ConnectionRefused => {
            "connection refused (nothing is listening on it)".to_string()
        }
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => e.to_string(),
    }
}

fn first_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Puts the runtime block after the `NOT RUN:` line of a step that was not run for want of a runtime: a line for the socket and the lines
/// of the output that concern the runtime, taken from the whole output, redacted, each behind [`RUNTIME_PREFIX`]; then the end of the output,
/// which gives way from the front, so the whole stays within `cap` bytes (the wire bound of a step's tail) and the end keeps at least
/// [`TAIL_FLOOR`] of them. A step that was not marked not run is left as it is.
pub fn add_runtime_block(step: &mut TestStep, socket: Option<&str>, lines: &[String], cap: usize) {
    if !step.is_not_run() || !step.output_tail.starts_with(NOT_RUN_PREFIX) {
        return;
    }
    let tail = std::mem::take(&mut step.output_tail);
    let (reason, rest) = match tail.split_once('\n') {
        Some((first, rest)) => (first.to_string(), rest.to_string()),
        None => (tail, String::new()),
    };
    let budget = cap.saturating_sub(reason.len() + 1 + TAIL_FLOOR);
    let mut block: Vec<String> = Vec::new();
    let mut used = 0usize;
    let socket_line = socket.map(str::to_string);
    for text in socket_line.iter().chain(lines.iter()) {
        let line = format!("{RUNTIME_PREFIX}{}", redact(text));
        if used + line.len() + 1 > budget {
            break;
        }
        used += line.len() + 1;
        block.push(line);
    }
    let mut head = reason;
    for line in &block {
        head.push('\n');
        head.push_str(line);
    }
    let room = cap.saturating_sub(head.len() + 1);
    let (kept, cut) = last_bytes(&rest, room);
    step.output_tail = if kept.is_empty() {
        head
    } else {
        format!("{head}\n{kept}")
    };
    step.output_truncated |= cut;
}

/// Marks a step that ran and failed for want of a container runtime, or of the program it starts, as not run: the reason first, then what it printed (the end of it, so the
/// whole stays within `cap` bytes). The exit code stays as the attempt gave it; the counts go, they measure nothing.
pub fn mark_not_run(result: &mut TestStep, reason: &str, cap: usize) {
    let head = format!("{NOT_RUN_PREFIX}{reason}\n");
    let room = cap.saturating_sub(head.len());
    let tail = &result.output_tail;
    let mut start = tail.len().saturating_sub(room);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    result.output_tail = format!("{head}{}", &tail[start..]);
    result.output_truncated |= start > 0;
    result.counts = None;
    result.counts_from = Some(NOT_RUN_MARK.to_string());
}

/// A small file of the engine's home (`dir/file`: the Maven mirror settings; a mirror of the package source, no secret) copied into the
/// steps' home. Only a regular file that is inside that home once its links are resolved: the engine owns the
/// home and could point the directory anywhere.
fn copy_home_file(env: &RunEnv, home: &Path, dir: &str, file: &str) {
    let Some(src_home) = &env.engine_home else {
        return;
    };
    let (Ok(real), Ok(root)) = (
        src_home.join(dir).join(file).canonicalize(),
        src_home.canonicalize(),
    ) else {
        return;
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return;
    };
    if !real.starts_with(&root) || !meta.is_file() || meta.len() > 64 * 1024 {
        return;
    }
    let dest_dir = home.join(dir);
    if std::fs::create_dir_all(&dest_dir).is_ok() {
        let _ = std::fs::copy(&real, dest_dir.join(file));
    }
}

/// The steps' `HOME` and `TMPDIR`, made once and given to the test user.
async fn prepare_dirs(env: &RunEnv, home: &Path, tmp: &Path) {
    if home.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(home);
    let _ = std::fs::create_dir_all(tmp);
    copy_home_file(env, home, ".m2", "settings.xml");
    // Cargo: the proxy's text, written by agentd into a directory it made; nothing of the engine's is read.
    if let Some(text) = &env.cargo_config {
        let dir = home.join(".cargo");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(dir.join("config.toml"), text);
        }
    }
    if let Some(owner) = env.run_as {
        crate::engine::chown_tree(&[home, tmp], owner).await;
    }
}

/// What a step starts once its command is read: the program with its words, and the variables its `NAME=value` prefixes add.
struct Launch {
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

/// Whether the shell reads `command` to its end (`sh -n`: parse, run nothing). Only a syntax error says no: dash and newer bash end with
/// exit code 2, bash 4.2 with 1, and say why on standard error; a shell that cannot be started ends with 126 or 127. The text is the first line
/// the shell printed. A shell that cannot be started, or a check that does not finish, says nothing about the command: the step runs and tells
/// what happens.
async fn shell_reads(env: &RunEnv, command: &str, cwd: &Path) -> Result<(), String> {
    // Parsing runs nothing, so it runs as the harness's own user: a change of user that fails (exit 1 from the launcher) is no syntax error.
    let spec = LaunchSpec {
        argv: vec!["sh".into(), "-n".into(), "-c".into(), command.to_string()],
        env: BTreeMap::new(),
        cwd: cwd.to_path_buf(),
        path: env.path.clone(),
        stderr_log: PathBuf::new(),
        run_as: None,
        sandbox: None,
    };
    let Ok(argv) = command_argv(&spec) else {
        return Ok(());
    };
    let Ok(joined) = std::env::join_paths(&spec.path) else {
        return Ok(());
    };
    let Ok(child) = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    else {
        return Ok(());
    };
    match tokio::time::timeout(READ_LIMIT, child.wait_with_output()).await {
        Ok(Ok(out)) if matches!(out.status.code(), Some(1 | 2)) => {
            // A syntax error comes with the shell's message; an exit 1 with no words is nothing to judge a command by.
            let text = String::from_utf8_lossy(&out.stderr);
            match text.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(first) => Err(first.chars().take(200).collect()),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// Reads `command` (the module `command`). Words run as an argument vector when their program is a file the test user may run and the kernel can
/// start (a binary, or a script with an interpreter line). Everything else goes to `sh -c`, once the shell itself has read it to the end: shell
/// syntax, a word the shell owns (`exit`, `echo`, `if`), a script with no interpreter line (the shell runs it as a shell script) and a program the
/// session lacks, so that the shell's own words ("not found", exit 127) reach the NOT RUN reading of a missing program. `Err` is the detail of a
/// command that cannot be read.
async fn read_command(env: &RunEnv, text: &str, cwd: &Path) -> Result<Launch, String> {
    if let Parsed::Plain {
        env: prefixes,
        mut argv,
    } = command::classify(text).map_err(|e| e.0)?
        && let Some(file) = command::find_program_as(&argv[0], &env.path, cwd, env.run_as)
        && command::runs_directly(&file)
    {
        // The file that was found is the file that starts, whatever the launcher's search would pick and wherever it starts the program from.
        argv[0] = file.display().to_string();
        return Ok(Launch {
            argv,
            env: prefixes,
        });
    }
    shell_reads(env, text, cwd).await?;
    Ok(Launch {
        argv: vec!["sh".into(), "-c".into(), text.to_string()],
        env: Vec::new(),
    })
}

/// Whether a finished step ends the run of steps because a setup did not give the tests what they need: a setup that failed or hit its limit,
/// and one whose command could not be read (the tests after an install that did not happen would fail for want of it: a red result the
/// repository did not earn). A setup that was not run for want of a service or of a program did not fail, and the tests go on.
pub fn setup_stops_the_run(step: &TestStep) -> bool {
    step.role == TestStepRole::Setup
        && if step.is_not_run() {
            step.not_run_reason() == Some(UNREADABLE)
        } else {
            step.timed_out || step.exit_code != Some(0)
        }
}

/// A step whose command could not be read: marked not run with [`UNREADABLE`] and the detail on the second line. It has no exit code, no counts
/// and no time, so nothing of it reads as a result of the suite.
fn unreadable(step: &PlannedStep, command: &str, detail: &str) -> TestStep {
    let mut result = TestStep::not_run(step.role, step.source, command, UNREADABLE, step.logged);
    result.output_tail = redact(&format!("{NOT_RUN_PREFIX}{UNREADABLE}\n{detail}"));
    result
}

fn blank(step: &PlannedStep) -> TestStep {
    TestStep {
        role: step.role,
        source: step.source,
        command: redact(&step.command),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        limit_s: None,
        counts: None,
        counts_from: None,
        output_tail: String::new(),
        output_truncated: false,
        logged: step.logged,
    }
}

async fn read_some<R: AsyncReadExt + Unpin>(r: &mut Option<R>, buf: &mut [u8]) -> usize {
    match r {
        Some(reader) => reader.read(buf).await.unwrap_or(0),
        None => 0,
    }
}

/// Runs one step in `cwd` for at most `limit`, keeping the last `tail_bytes` of its output.
pub async fn run_step(
    env: &RunEnv,
    step: &PlannedStep,
    cwd: &Path,
    limit: Duration,
    tail_bytes: usize,
) -> TestStep {
    run_step_scanned(env, step, cwd, limit, tail_bytes).await.0
}

/// Runs a helper of the report (`ham-services env`, `ham-services stop`) as the steps run, and returns what it printed to standard output, or `None`
/// when it could not be started, did not end with 0 or did not end in `limit`. Nothing is redacted: the caller reads values that the steps need.
pub async fn capture(env: &RunEnv, command: &str, cwd: &Path, limit: Duration) -> Option<String> {
    let launch = read_command(env, command, cwd).await.ok()?;
    let home = env.work.join("home");
    let tmp = env.work.join("tmp");
    prepare_dirs(env, &home, &tmp).await;
    let mut vars = step_env(env, &home, &tmp);
    vars.extend(launch.env);
    let spec = LaunchSpec {
        argv: launch.argv,
        env: vars,
        cwd: cwd.to_path_buf(),
        path: env.path.clone(),
        stderr_log: PathBuf::new(),
        run_as: env.run_as,
        sandbox: env.sandbox.clone(),
    };
    let argv = command_argv(&spec).ok()?;
    let joined = std::env::join_paths(&spec.path).ok()?;
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let out = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .chars()
            .take(8 * 1024)
            .collect()
    })
}

/// [`run_step`] and the ports of this machine the output said a connection was refused to ([`crate::direct::report::parse::refused_port`]).
pub async fn run_step_scanned(
    env: &RunEnv,
    step: &PlannedStep,
    cwd: &Path,
    limit: Duration,
    tail_bytes: usize,
) -> (TestStep, Vec<u16>) {
    let started = Instant::now();
    let mut result = blank(step);
    // A command that starts a container tool in a session without a runtime is not started: it could only fail, and a failure the session
    // caused is not the repository's.
    if let Some(why) = not_run_before(env, &step.command) {
        return (
            TestStep::not_run(step.role, step.source, &result.command, why, step.logged),
            Vec::new(),
        );
    }
    // The command is read before anything is made or started: one that cannot be read is not run.
    let launch = match read_command(env, &step.command, cwd).await {
        Ok(l) => l,
        Err(detail) => return (unreadable(step, &result.command, &detail), Vec::new()),
    };
    let home = env.work.join("home");
    let tmp = env.work.join("tmp");
    prepare_dirs(env, &home, &tmp).await;
    let mut vars = step_env(env, &home, &tmp);
    vars.extend(launch.env);
    let spec = LaunchSpec {
        argv: launch.argv,
        env: vars,
        cwd: cwd.to_path_buf(),
        path: env.path.clone(),
        stderr_log: PathBuf::new(),
        run_as: env.run_as,
        sandbox: env.sandbox.clone(),
    };
    let argv = match command_argv(&spec) {
        Ok(a) => a,
        Err(e) => {
            result.output_tail = format!("the command could not be started: {e}");
            return (result, Vec::new());
        }
    };
    let joined = match std::env::join_paths(&spec.path) {
        Ok(j) => j,
        Err(e) => {
            result.output_tail = format!("the command could not be started: {e}");
            return (result, Vec::new());
        }
    };
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            result.output_tail = redact(&format!("the command could not be started: {e}"));
            return (result, Vec::new());
        }
    };
    let pid = child.id();
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let mut scan = OutputScan::new(tail_bytes);
    let mut buf_o = vec![0u8; 8192];
    let mut buf_e = vec![0u8; 8192];
    let mut status = None;
    let mut timed_out = false;
    // When the command ended (or was stopped) the pipes are read for a moment more, whoever still holds them.
    let mut drain_until: Option<tokio::time::Instant> = None;
    let sleep = tokio::time::sleep(limit);
    tokio::pin!(sleep);
    loop {
        let drain_at = drain_until;
        let drain_over = async move {
            match drain_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            n = read_some(&mut out, &mut buf_o), if out.is_some() => {
                if n == 0 { out = None } else { scan.feed(&buf_o[..n]) }
            }
            n = read_some(&mut err, &mut buf_e), if err.is_some() => {
                if n == 0 { err = None } else { scan.feed(&buf_e[..n]) }
            }
            st = child.wait(), if status.is_none() => {
                status = st.ok();
                // The command ended: whatever it left running in its group goes with it, and the pipes close.
                if let Some(p) = pid { kill_group_of(p).await; }
                drain_until.get_or_insert(tokio::time::Instant::now() + DRAIN);
            }
            () = &mut sleep, if !timed_out && status.is_none() => {
                timed_out = true;
                if let Some(p) = pid { kill_group_of(p).await; }
                let _ = child.start_kill();
                drain_until.get_or_insert(tokio::time::Instant::now() + DRAIN);
            }
            () = drain_over => break,
        }
        if out.is_none() && err.is_none() && status.is_some() {
            break;
        }
    }
    if status.is_none() {
        // Stopped at its limit: reaped within a moment, as the group is gone.
        status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .ok()
            .and_then(Result::ok);
    }
    result.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result.timed_out = timed_out;
    if !timed_out {
        result.exit_code = status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|sig| 128 + sig))
        });
    }
    let scanned = scan.finish(&step.command);
    let missing_runtime = scanned.container_runtime_missing;
    let missing_program = scanned.missing_program.clone();
    let out_of_disk = scanned.out_of_disk.clone();
    let refused_ports = scanned.refused_ports.clone();
    let runtime_lines = scanned.runtime_lines.clone();
    result.output_tail = redact(&scanned.tail);
    result.output_truncated = scanned.truncated;
    if let Some((runner, counts)) = scanned.counts {
        result.counts = Some(counts);
        result.counts_from = Some(runner.to_string());
    }
    // A command that failed and said it found no container runtime to talk to was stopped by the session, not by the repository's code:
    // not red. A command that exited 0, or that did not say so, is judged by its exit code as before.
    if !timed_out && result.exit_code.is_some_and(|c| c != 0) && missing_runtime {
        let why = if env.has_container_runtime() {
            RUNTIME_DOWN
        } else {
            NO_RUNTIME
        };
        mark_not_run(&mut result, why, tail_bytes);
        // The tail of a step holds the end of its output; Testcontainers prints what it tried near the start. The lines about the runtime come from
        // the whole output, with what the runtime's socket does now.
        let socket = runtime_socket_state(&env.engine_env).await;
        add_runtime_block(&mut result, socket.as_deref(), &runtime_lines, tail_bytes);
    } else if let Some(program) = program_missing_here(
        &step.command,
        result.exit_code,
        timed_out,
        missing_program.as_deref(),
    ) {
        // The report's environment has no such program (the direct tool set has no Rust toolchain, for one): a fact about the environment and
        // no test result, so the step says so and the card does not show an exit code as a failure of the repository's tests.
        mark_not_run(
            &mut result,
            &format!("{program} is not installed here"),
            tail_bytes,
        );
    } else if let Some(line) = disk_full_here(
        result.exit_code,
        timed_out,
        result.counts,
        out_of_disk.as_deref(),
    ) {
        // The session's volume is full (a whole-workspace debug build of a big repository can fill it): a fact about the session and no test
        // result, so the step says so with the line that names it, and the card does not say that a test exited 101.
        mark_not_run(
            &mut result,
            &format!("{OUT_OF_DISK}: {}", redact(line)),
            tail_bytes,
        );
    }
    (result, refused_ports)
}
