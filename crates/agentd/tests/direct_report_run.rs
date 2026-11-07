//! Module `report`: the clean checkout and the commands run in it. Every command here is harmless: `true`, an exit code, `echo`
//! of plain text, `sleep`, `pwd`, `env`. The checks are on what the step recorded (exit code, counts, tail, time), on what the step could
//! see (its environment and directory) and on how a step that never ends is stopped.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::plan::PlannedStep;
use agentd::direct::report::run::{RunEnv, checkout, run_step};
use harness_protocol::direct::{TestCommandSource, TestCounts, TestStepRole};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn env_for(work: &Path) -> RunEnv {
    RunEnv {
        sandbox: None,
        work: work.to_path_buf(),
        run_as: None,
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

fn step(command: &str) -> PlannedStep {
    PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::TestCommandFile,
        command: command.to_string(),
        logged: None,
    }
}

async fn run(command: &str, limit_s: u64) -> harness_protocol::direct::TestStep {
    let work = common::scratch("report-run");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    run_step(
        &env_for(&work),
        &step(command),
        &cwd,
        Duration::from_secs(limit_s),
        4096,
    )
    .await
}

#[tokio::test]
async fn a_command_that_succeeds_records_exit_zero_and_its_time() {
    let s = run("true", 30).await;
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.timed_out);
    assert_eq!(s.command, "true");
    assert_eq!(s.counts, None);
    assert!(s.duration_ms < 20_000, "{}", s.duration_ms);
}

#[tokio::test]
async fn a_failing_exit_code_is_recorded_as_it_is() {
    assert_eq!(run("exit 3", 30).await.exit_code, Some(3));
    assert_eq!(run("false", 30).await.exit_code, Some(1));
}

#[tokio::test]
async fn the_output_of_both_streams_is_kept_and_its_summary_is_counted() {
    let s = run("echo \"setup noise\"; echo \"warning on stderr\" 1>&2; echo \"4 passed, 1 failed in 0.02s\"", 30).await;
    assert_eq!(s.exit_code, Some(0));
    assert!(s.output_tail.contains("setup noise"), "{:?}", s.output_tail);
    assert!(
        s.output_tail.contains("warning on stderr"),
        "{:?}",
        s.output_tail
    );
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 4,
            failed: 1,
            skipped: 0
        })
    );
    assert_eq!(s.counts_from.as_deref(), Some("pytest"));
}

#[tokio::test]
async fn a_step_that_never_ends_is_stopped_at_its_limit() {
    let t = Instant::now();
    let s = run("sleep 60", 1).await;
    assert!(s.timed_out);
    assert_eq!(s.exit_code, None);
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
}

#[tokio::test]
async fn a_background_process_that_keeps_the_pipes_open_does_not_hold_the_step() {
    let t = Instant::now();
    let s = run("sleep 60 & echo started", 20).await;
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.timed_out);
    assert!(s.output_tail.contains("started"), "{:?}", s.output_tail);
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
}

#[tokio::test]
async fn a_flood_of_output_is_cut_to_the_tail_cap() {
    let s = run("yes x | head -n 300000; echo done", 60).await;
    assert_eq!(s.exit_code, Some(0));
    assert!(s.output_tail.len() <= 4096, "{}", s.output_tail.len());
    assert!(s.output_tail.ends_with("done\n"));
    assert!(s.output_truncated);
}

#[tokio::test]
async fn the_step_runs_in_the_directory_it_is_given_with_nothing_on_its_input() {
    let work = common::scratch("report-cwd");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let s = run_step(
        &env_for(&work),
        &step("pwd; cat"),
        &cwd,
        Duration::from_secs(30),
        4096,
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "an empty input ends cat at once");
    assert!(
        s.output_tail
            .starts_with(cwd.canonicalize().unwrap().to_str().unwrap()),
        "{:?}",
        s.output_tail
    );
}

#[tokio::test]
async fn the_step_sees_the_package_sources_and_a_clean_home_and_none_of_the_engines_other_settings()
{
    let work = common::scratch("report-env");
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut env = env_for(&work);
    env.engine_env = [
        ("PIP_INDEX_URL", "http://127.0.0.1:3128/pypi/simple/"),
        ("npm_config_registry", "http://127.0.0.1:3128/npm/"),
        ("GOPROXY", "http://127.0.0.1:3128/go"),
        ("AWS_PROFILE", "agent"),
        ("AWS_CONFIG_FILE", "/engine/aws/config"),
        ("AGENTD_SOMETHING", "x"),
        ("HOME", "/engine/home"),
        ("CLINE_GLOBAL_SETTINGS_PATH", "/engine/settings.json"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let s = run_step(&env, &step("env"), &cwd, Duration::from_secs(30), 32 * 1024).await;
    let out = s.output_tail;
    assert!(
        out.contains("PIP_INDEX_URL=http://127.0.0.1:3128/pypi/simple/"),
        "{out}"
    );
    assert!(
        out.contains("npm_config_registry=http://127.0.0.1:3128/npm/"),
        "{out}"
    );
    assert!(out.contains("GOPROXY=http://127.0.0.1:3128/go"), "{out}");
    assert!(
        out.contains(&format!("HOME={}", work.join("home").display())),
        "{out}"
    );
    assert!(
        out.contains(&format!("TMPDIR={}", work.join("tmp").display())),
        "{out}"
    );
    assert!(out.contains("CI=1"), "{out}");
    for left_out in [
        "AWS_PROFILE",
        "AWS_CONFIG_FILE",
        "AGENTD_",
        "/engine/home",
        "CLINE_GLOBAL_SETTINGS_PATH",
    ] {
        assert!(
            !out.contains(left_out),
            "{left_out} reached the step: {out}"
        );
    }
}

#[tokio::test]
async fn a_maven_settings_file_of_the_engine_home_is_copied_into_the_steps_home() {
    let work = common::scratch("report-m2");
    let engine_home = common::scratch("report-m2-home");
    std::fs::create_dir_all(engine_home.join(".m2")).unwrap();
    std::fs::write(engine_home.join(".m2/settings.xml"), "<settings/>").unwrap();
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut env = env_for(&work);
    env.engine_home = Some(engine_home);
    let s = run_step(
        &env,
        &step("cat \"$HOME/.m2/settings.xml\""),
        &cwd,
        Duration::from_secs(30),
        4096,
    )
    .await;
    assert_eq!(s.exit_code, Some(0));
    assert_eq!(s.output_tail, "<settings/>");
}

#[tokio::test]
async fn a_secret_in_the_command_or_the_output_is_replaced_before_either_is_kept() {
    // A made-up key id, built at run time, the shape the redaction knows.
    let key = common::canary_key_id();
    let s = run(&format!("echo \"the id is {key}\""), 30).await;
    assert_eq!(s.exit_code, Some(0));
    assert!(!s.output_tail.contains(&key), "{:?}", s.output_tail);
    assert!(!s.command.contains(&key), "{:?}", s.command);
    assert!(s.output_tail.contains("<redacted>"), "{:?}", s.output_tail);
}

// ---- the checkout ------------------------------------------------------------------------------------------------------------

fn repo_with_bundle() -> (PathBuf, PathBuf, String, String) {
    let dir = common::scratch("report-bundle");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    common::git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    common::git(&repo, &["add", "."]);
    common::git(&repo, &["commit", "-q", "-m", "first"]);
    let first = common::git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    std::fs::write(repo.join("b.txt"), "extra\n").unwrap();
    common::git(&repo, &["add", "."]);
    common::git(&repo, &["commit", "-q", "-m", "second"]);
    let head = common::git(&repo, &["rev-parse", "HEAD"]);
    let bundle = dir.join("repo.bundle");
    common::git(
        &repo,
        &["bundle", "create", "-q", bundle.to_str().unwrap(), "main"],
    );
    // An uncommitted change in the original tree must not reach the clean checkout.
    std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    (dir, bundle, head, first)
}

#[tokio::test]
async fn the_checkout_holds_exactly_the_committed_head() {
    let (dir, bundle, head, _) = repo_with_bundle();
    let dest = dir.join("clean");
    checkout(&bundle, &head, &dest).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("a.txt")).unwrap(),
        "two\n"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("b.txt")).unwrap(),
        "extra\n"
    );
    assert_eq!(common::git(&dest, &["rev-parse", "HEAD"]), head);
    assert_eq!(common::git(&dest, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn an_earlier_head_of_the_same_bundle_checks_out_that_commit() {
    let (dir, bundle, _, first) = repo_with_bundle();
    let dest = dir.join("clean-first");
    checkout(&bundle, &first, &dest).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!dest.join("b.txt").exists());
}

#[tokio::test]
async fn a_head_the_bundle_lacks_or_a_missing_bundle_is_an_error_and_not_a_checkout() {
    let (dir, bundle, _, _) = repo_with_bundle();
    let err = checkout(&bundle, &"0".repeat(40), &dir.join("clean-bad"))
        .await
        .unwrap_err();
    assert!(!err.is_empty());
    let err2 = checkout(
        &dir.join("nope.bundle"),
        &"0".repeat(40),
        &dir.join("clean-bad2"),
    )
    .await
    .unwrap_err();
    assert!(!err2.is_empty());
}

#[tokio::test]
async fn a_step_has_the_metadata_service_switched_off_like_every_engine_process() {
    // Invariant 2: no credential comes from instance metadata. The engine gets AWS_EC2_METADATA_DISABLED=true and so does the worker
    // image; a test run in the clean checkout must not fall back to the metadata endpoint either, whatever the engine's environment held.
    for engine_value in [None, Some("false"), Some("true")] {
        let work = common::scratch("report-metadata");
        let cwd = work.join("checkout");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut env = env_for(&work);
        if let Some(v) = engine_value {
            env.engine_env
                .insert("AWS_EC2_METADATA_DISABLED".to_string(), v.to_string());
        }
        let s = run_step(&env, &step("env"), &cwd, Duration::from_secs(30), 32 * 1024).await;
        assert!(
            s.output_tail
                .lines()
                .any(|l| l == "AWS_EC2_METADATA_DISABLED=true"),
            "{engine_value:?}: {}",
            s.output_tail
        );
    }
}

// ---- what a step stops: its own process group, and nothing else -------------------------------------------
//
// The container runtime of the session (the Docker front and Podman's service) runs as the same user as the steps. A step must leave a
// process of that user that it did not start alone, whatever the step does and however it ends.

/// A long-lived process of this user that no step started: the stand-in for the container runtime's front.
struct Bystander(std::process::Child);

impl Bystander {
    fn start() -> Self {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
}

impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

#[tokio::test]
async fn a_step_does_not_stop_a_process_of_the_same_user_that_it_did_not_start() {
    let bystander = Bystander::start();
    let pid = bystander.0.id();
    assert!(alive(pid), "TRIGGER PRESENT: the stand-in runtime runs");
    // Setup, tests and a step cut off at its limit, one after the other as a plan runs them.
    assert_eq!(run("echo setup", 30).await.exit_code, Some(0));
    assert!(
        alive(pid),
        "a step that ended by itself stopped a process it did not start"
    );
    assert_eq!(run("exit 1", 30).await.exit_code, Some(1));
    assert!(
        alive(pid),
        "a failing step stopped a process it did not start"
    );
    assert!(run("sleep 60", 1).await.timed_out);
    assert!(
        alive(pid),
        "a step cut off at its limit stopped a process it did not start"
    );
    // A step that starts a background job of its own: the job goes with the step's group, the bystander stays.
    assert_eq!(run("sleep 200 & echo started", 30).await.exit_code, Some(0));
    assert!(
        alive(pid),
        "a step with a background job stopped a process it did not start"
    );
}

// ---- the report step gets the environment of Cline's own session, and says NOT RUN where a service is missing -----
//
// Cline's session has a Maven settings file that mirrors to the loopback package proxy (named by `MAVEN_ARGS`, because Java's `user.home` is not
// `$HOME`), the package-tool variables and the four variables of the session's container runtime. The report step runs the same commands in a
// clean checkout, so it gets the same: package settings and the runtime's variables, and no credential (none of those holds one).

fn engine_with_maven_settings(home: &Path) -> BTreeMap<String, String> {
    let settings = home.join(".m2/settings.xml");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        "<settings><mirrors><mirror><id>loop</id><mirrorOf>*</mirrorOf><url>http://127.0.0.1:3128/maven/</url></mirror></mirrors></settings>",
    )
    .unwrap();
    [(
        "MAVEN_ARGS".to_string(),
        format!("--settings {}", settings.display()),
    )]
    .into()
}

/// What the package proxy gave the run as its cargo config (the real text is `pkgproxy::Handle::cargo_config_toml`).
const PROXY_CARGO_TOML: &str = "[source.crates-io]\nreplace-with = \"hamh-proxy\"\n\n[registries.hamh-proxy]\nindex = \"sparse+http://127.0.0.1:3128/cargo/\"\n";

/// A config file the ENGINE wrote in its cargo home: it can set a compiler wrapper and a test runner, so no step may read it.
const ENGINE_CARGO_TOML: &str = "[build]\nrustc-wrapper = \"/engine/wrapper\"\n";

/// The engine's cargo variables of a session with a package proxy: its home, which holds a file the engine wrote, and the build switches.
fn engine_with_cargo_config(home: &Path) -> BTreeMap<String, String> {
    let cargo_home = home.join(".cargo");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::write(cargo_home.join("config.toml"), ENGINE_CARGO_TOML).unwrap();
    [
        ("CARGO_HOME", cargo_home.display().to_string()),
        ("CARGO_BUILD_JOBS", "2".to_string()),
        ("CARGO_PROFILE_DEV_DEBUG", "0".to_string()),
        ("CARGO_PROFILE_TEST_DEBUG", "0".to_string()),
        ("CARGO_INCREMENTAL", "0".to_string()),
        ("RUST_BACKTRACE", "1".to_string()),
        ("CARGO_NET_GIT_FETCH_WITH_CLI", "false".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn the_cargo_switches_pass_to_the_steps_and_a_registry_token_never_does() {
    for name in [
        "CARGO_BUILD_JOBS",
        "CARGO_PROFILE_DEV_DEBUG",
        "CARGO_PROFILE_TEST_DEBUG",
        "CARGO_INCREMENTAL",
        "RUST_BACKTRACE",
        "CARGO_NET_GIT_FETCH_WITH_CLI",
    ] {
        assert!(agentd::direct::report::run::passes_to_steps(name), "{name}");
    }
    for name in [
        // the engine's own cargo home is never the steps': they get one made from the proxy's text
        "CARGO_HOME",
        "CARGO_REGISTRIES_CRATES_IO_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "CARGO_HTTP_PROXY",
        "CARGO_TARGET_DIR",
        "RUSTC_WRAPPER",
    ] {
        assert!(
            !agentd::direct::report::run::passes_to_steps(name),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_step_gets_the_cargo_config_the_proxy_gave_and_never_a_file_the_engine_wrote() {
    let work = common::scratch("report-cargo");
    let engine_home = common::scratch("report-cargo-engine-home");
    let mut env = env_for(&work);
    env.engine_env = engine_with_cargo_config(&engine_home);
    env.engine_home = Some(engine_home);
    env.cargo_config = Some(PROXY_CARGO_TOML.to_string());
    let s = run_in(
        &env,
        &work,
        "echo \"$CARGO_HOME\"; cat \"$CARGO_HOME/config.toml\"; echo \"jobs=$CARGO_BUILD_JOBS inc=$CARGO_INCREMENTAL\"",
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    let (first, rest) = s.output_tail.split_once('\n').unwrap();
    // The step's own home, with the proxy's text: not the engine's directory, and nothing the engine wrote.
    assert_eq!(first, work.join("home/.cargo").display().to_string());
    assert_eq!(rest, format!("{PROXY_CARGO_TOML}jobs=2 inc=0\n"));
    assert!(
        !s.output_tail.contains("rustc-wrapper"),
        "{}",
        s.output_tail
    );
}

#[tokio::test]
async fn without_a_cargo_config_no_cargo_home_reaches_the_step_but_the_switches_do() {
    let work = common::scratch("report-cargo-none");
    let mut env = env_for(&work);
    env.engine_env = [
        ("CARGO_HOME".to_string(), "/gone/.cargo".to_string()),
        ("CARGO_BUILD_JOBS".to_string(), "2".to_string()),
    ]
    .into();
    env.engine_home = Some(common::scratch("report-cargo-none-home"));
    let s = run_in(&env, &work, "env").await;
    assert!(!s.output_tail.contains("CARGO_HOME"), "{}", s.output_tail);
    assert!(
        s.output_tail.contains("CARGO_BUILD_JOBS=2"),
        "{}",
        s.output_tail
    );
}

#[tokio::test]
async fn a_cargo_directory_the_engine_planted_changes_nothing_in_the_steps() {
    let work = common::scratch("report-cargo-link");
    let engine_home = common::scratch("report-cargo-link-home");
    let outside = common::scratch("report-cargo-link-outside");
    std::fs::write(outside.join("config.toml"), ENGINE_CARGO_TOML).unwrap();
    std::os::unix::fs::symlink(&outside, engine_home.join(".cargo")).unwrap();
    let mut env = env_for(&work);
    env.engine_env = [(
        "CARGO_HOME".to_string(),
        engine_home.join(".cargo").display().to_string(),
    )]
    .into();
    env.engine_home = Some(engine_home);
    env.cargo_config = Some(PROXY_CARGO_TOML.to_string());
    let s = run_in(&env, &work, "cat \"$CARGO_HOME/config.toml\"").await;
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    assert_eq!(s.output_tail, PROXY_CARGO_TOML);
    assert_eq!(
        std::fs::read_to_string(outside.join("config.toml")).unwrap(),
        ENGINE_CARGO_TOML,
        "nothing was written through the link"
    );
}

#[tokio::test]
async fn no_proxy_text_means_no_cargo_home_even_when_the_engine_has_a_config_file() {
    let work = common::scratch("report-cargo-no-text");
    let engine_home = common::scratch("report-cargo-no-text-home");
    let mut env = env_for(&work);
    env.engine_env = engine_with_cargo_config(&engine_home);
    env.engine_home = Some(engine_home);
    let s = run_in(&env, &work, "env; ls -a \"$HOME\"").await;
    assert!(!s.output_tail.contains("CARGO_HOME"), "{}", s.output_tail);
    assert!(!work.join("home/.cargo").exists());
}

const RUNTIME_VARS: [(&str, &str); 4] = [
    ("CONTAINER_HOST", "unix:///var/run/docker.sock"),
    ("DOCKER_HOST", "unix:///var/run/docker.sock"),
    ("TESTCONTAINERS_HOST_OVERRIDE", "127.0.0.1"),
    ("TESTCONTAINERS_RYUK_DISABLED", "true"),
];

/// A step agentd did not run, or whose missing service decided it: the marker agentd puts on it (never the repository's own output).
fn not_run(s: &harness_protocol::direct::TestStep) -> bool {
    s.counts_from.as_deref() == Some("not run")
}

async fn run_in(env: &RunEnv, work: &Path, command: &str) -> harness_protocol::direct::TestStep {
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    run_step(env, &step(command), &cwd, Duration::from_secs(30), 4096).await
}

#[tokio::test]
async fn a_step_reads_the_maven_settings_of_the_session_through_its_own_copy() {
    let work = common::scratch("report-maven");
    let engine_home = common::scratch("report-maven-engine-home");
    let mut env = env_for(&work);
    env.engine_env = engine_with_maven_settings(&engine_home);
    env.engine_home = Some(engine_home);
    let s = run_in(
        &env,
        &work,
        "set -- $MAVEN_ARGS; test \"$1\" = --settings && grep -q \"<mirror>\" \"$2\" && echo \"$2\"",
    )
    .await;
    assert_eq!(s.exit_code, Some(0), "{}", s.output_tail);
    // The copy in the step's own home, not the engine's file: the step cannot change what the engine reads.
    let named = s.output_tail.trim();
    assert_eq!(
        named,
        work.join("home/.m2/settings.xml").display().to_string()
    );
}

#[tokio::test]
async fn without_a_settings_file_no_maven_option_reaches_the_step() {
    let work = common::scratch("report-maven-none");
    let mut env = env_for(&work);
    env.engine_env = [(
        "MAVEN_ARGS".to_string(),
        "--settings /gone/settings.xml".to_string(),
    )]
    .into();
    env.engine_home = Some(common::scratch("report-maven-none-home"));
    let s = run_in(&env, &work, "env").await;
    assert!(!s.output_tail.contains("MAVEN_ARGS"), "{}", s.output_tail);
}

#[tokio::test]
async fn a_step_gets_the_container_variables_of_the_session_and_no_credential() {
    let work = common::scratch("report-docker-env");
    let mut env = env_for(&work);
    env.engine_env = RUNTIME_VARS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain([
            (
                "AWS_ACCESS_KEY_ID".to_string(),
                "ASIA-test-not-a-key".to_string(),
            ),
            ("AWS_SESSION_TOKEN".to_string(), "not-a-token".to_string()),
            ("AWS_PROFILE".to_string(), "agent".to_string()),
        ])
        .collect();
    let s = run_in(&env, &work, "env").await;
    for (k, v) in RUNTIME_VARS {
        assert!(
            s.output_tail.lines().any(|l| l == format!("{k}={v}")),
            "{k}: {}",
            s.output_tail
        );
    }
    for left_out in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "not-a-token",
    ] {
        assert!(
            !s.output_tail.contains(left_out),
            "{left_out} reached the step: {}",
            s.output_tail
        );
    }
}

#[tokio::test]
async fn a_command_that_needs_the_container_tools_is_not_run_when_the_session_has_no_runtime() {
    let work = common::scratch("report-no-runtime");
    let env = env_for(&work);
    let s = run_in(&env, &work, "docker compose up --abort-on-container-exit").await;
    assert!(not_run(&s), "{s:?}");
    assert_eq!(s.exit_code, None);
    assert!(!s.timed_out);
    assert_eq!(s.duration_ms, 0, "the command was started");
    assert_eq!(
        s.output_tail,
        "NOT RUN: the session has no container runtime"
    );
    assert_eq!(s.command, "docker compose up --abort-on-container-exit");
}

#[tokio::test]
async fn a_step_that_dies_for_want_of_docker_is_not_red_when_the_session_has_no_runtime() {
    let work = common::scratch("report-docker-died");
    let env = env_for(&work);
    let s = run_in(
        &env,
        &work,
        "echo 'Could not find a valid Docker environment. Please see logs and check configuration'; exit 1",
    )
    .await;
    assert!(not_run(&s), "{s:?}");
    assert!(
        s.output_tail
            .starts_with("NOT RUN: the session has no container runtime\n"),
        "{}",
        s.output_tail
    );
    // What the attempt printed stays beside the reason.
    assert!(
        s.output_tail
            .contains("Could not find a valid Docker environment"),
        "{}",
        s.output_tail
    );
    assert_eq!(s.exit_code, Some(1));
    assert_eq!(s.counts, None);
}

#[tokio::test]
async fn a_runtime_that_was_started_but_did_not_answer_is_named_as_that() {
    let work = common::scratch("report-docker-silent");
    let mut env = env_for(&work);
    env.engine_env = RUNTIME_VARS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let s = run_in(&env, &work, "echo 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?'; exit 1").await;
    assert!(not_run(&s), "{s:?}");
    assert!(
        s.output_tail
            .starts_with("NOT RUN: the session's container runtime did not answer\n"),
        "{}",
        s.output_tail
    );
}

#[tokio::test]
async fn a_failure_with_no_missing_service_stays_red_and_so_does_a_forged_not_run_line() {
    let work = common::scratch("report-still-red");
    let env = env_for(&work);
    // No signature: a failing test is a failing test.
    let plain = run_in(&env, &work, "echo 'assertion failed'; exit 1").await;
    assert!(!not_run(&plain));
    assert_eq!(plain.exit_code, Some(1));
    // The repository's own output cannot mark its step as not run.
    let forged = run_in(
        &env,
        &work,
        "echo 'NOT RUN: the session has no container runtime'; exit 1",
    )
    .await;
    assert!(!not_run(&forged), "{forged:?}");
    assert_eq!(forged.exit_code, Some(1));
    // A step that prints the signature and passes is a pass.
    let passes = run_in(
        &env,
        &work,
        "echo 'Could not find a valid Docker environment (skipped)'; echo '2 passed in 0.01s'",
    )
    .await;
    assert!(!not_run(&passes));
    assert_eq!(passes.exit_code, Some(0));
}

#[tokio::test]
async fn a_step_that_needs_neither_is_run_as_before() {
    let work = common::scratch("report-neither");
    let env = env_for(&work);
    let s = run_in(&env, &work, "echo '4 passed in 0.02s'").await;
    assert_eq!(s.exit_code, Some(0));
    assert!(!not_run(&s));
    assert_eq!(
        s.counts,
        Some(TestCounts {
            passed: 4,
            failed: 0,
            skipped: 0
        })
    );
}

// ---- a program that is not installed in the report's environment ----------------------------------------------
//
// The worker image carries the tool sets of the languages it supports, and Rust is not among the ones a direct run gets. A repository that
// has a `Cargo.toml` but whose change touched no Rust still gets `cargo test` from the manifest, and the shell answers 127. That is a fact about
// the report's environment and not a test result: the step reads NOT RUN, and the card does not say that a test exited 127.

const MISSING: &str = "no-such-tool-for-the-report";

#[tokio::test]
async fn a_command_whose_program_is_not_installed_is_not_run_and_names_the_program() {
    let work = common::scratch("report-missing-program");
    let env = env_for(&work);
    let s = run_in(&env, &work, &format!("{MISSING} test --workspace")).await;
    assert!(not_run(&s), "{s:?}");
    assert_eq!(s.exit_code, Some(127), "the attempt's own code stays");
    assert_eq!(s.counts, None);
    assert!(
        s.output_tail
            .starts_with(&format!("NOT RUN: {MISSING} is not installed here\n")),
        "{:?}",
        s.output_tail
    );
    assert_eq!(s.command, format!("{MISSING} test --workspace"));
}

#[tokio::test]
async fn the_program_after_a_leading_cd_and_after_assignments_counts_as_the_commands_program() {
    let work = common::scratch("report-missing-program-cd");
    let env = env_for(&work);
    for command in [
        format!("cd . && {MISSING} test"),
        format!("CI=1 {MISSING} test"),
        format!("cd . && CI=1 {MISSING} test"),
    ] {
        let s = run_in(&env, &work, &command).await;
        assert!(not_run(&s), "{command}: {s:?}");
    }
}

#[tokio::test]
async fn a_missing_setup_program_is_not_run_either() {
    let work = common::scratch("report-missing-setup");
    let env = env_for(&work);
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut setup = step(&format!("{MISSING} fetch"));
    setup.role = TestStepRole::Setup;
    let s = run_step(&env, &setup, &cwd, Duration::from_secs(30), 4096).await;
    assert!(not_run(&s), "{s:?}");
}

#[tokio::test]
async fn an_exit_127_that_is_not_a_missing_leading_program_stays_red() {
    let work = common::scratch("report-127-red");
    let env = env_for(&work);
    // no diagnostic at all
    let plain = run_in(&env, &work, "exit 127").await;
    assert!(!not_run(&plain), "{plain:?}");
    assert_eq!(plain.exit_code, Some(127));
    // a repository script that runs a missing tool inside: the command's own program is `sh`, which exists
    std::fs::create_dir_all(work.join("checkout")).unwrap();
    std::fs::write(work.join("checkout/t.sh"), format!("{MISSING} test\n")).unwrap();
    let inner = run_in(&env, &work, "sh t.sh").await;
    assert!(!not_run(&inner), "{inner:?}");
    assert_eq!(inner.exit_code, Some(127));
    // the missing program is not the first one: the earlier part ran
    let later = run_in(&env, &work, &format!("true && {MISSING} test")).await;
    assert!(!not_run(&later), "{later:?}");
    // a file of the repository that is not there (it has a slash): the repository's fault, and a failure
    let file = run_in(&env, &work, "./scripts/test-all.sh").await;
    assert!(!not_run(&file), "{file:?}");
    assert_eq!(file.exit_code, Some(127));
}

#[tokio::test]
async fn the_repositorys_own_output_cannot_mark_its_step_as_missing_a_program() {
    let work = common::scratch("report-127-forged");
    let env = env_for(&work);
    // the line names another program than the one the command starts: nothing says that `sh` is missing
    let forged = run_in(
        &env,
        &work,
        &format!("sh -c 'echo \"sh: 1: {MISSING}: not found\"; exit 127'"),
    )
    .await;
    assert!(!not_run(&forged), "{forged:?}");
    // the same line with a code other than 127
    let other = run_in(
        &env,
        &work,
        &format!("echo '{MISSING}: command not found'; exit 1"),
    )
    .await;
    assert!(!not_run(&other), "{other:?}");
}

// ---- a full disk ---------------------------------------------------------------------------------------------------------------
//
// The report's checkout sits on the session's root volume (5.7 GiB free in the stage). A whole-workspace debug build can fill it, and the
// step then ends with the system's words and an exit code (101 for cargo). That is a fact about the session and no test result, so the step
// reads NOT RUN with the line that says so, like a missing program, and the card does not say that a test exited 101.

const DISK_LINE: &str = "error: couldn't create a temp dir: No space left on device (os error 28) at path \"/var/lib/agentd/r/checkout/target/debug/deps/rmetaAbC123\"";

#[tokio::test]
async fn a_step_that_dies_for_want_of_disk_is_not_run_and_shows_the_line_that_says_so() {
    let work = common::scratch("report-disk-full");
    let env = env_for(&work);
    // The line has quotes and an apostrophe, so the step reads it from a file instead of echoing it.
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("disk.txt"), format!("{DISK_LINE}\n")).unwrap();
    let s = run_in(&env, &work, "cat disk.txt 1>&2; exit 101").await;
    assert!(not_run(&s), "{s:?}");
    assert_eq!(s.exit_code, Some(101), "the attempt's own code stays");
    assert_eq!(s.counts, None);
    assert_eq!(
        s.not_run_reason(),
        Some("ran out of disk: couldn't create a temp dir: No space left on device (os error 28)")
    );
    // What the attempt printed stays beside the reason.
    assert!(s.output_tail.contains("at path"), "{}", s.output_tail);
}

#[tokio::test]
async fn the_words_of_a_full_disk_change_nothing_for_a_step_that_passes_times_out_or_found_failing_tests()
 {
    let work = common::scratch("report-disk-words");
    let env = env_for(&work);
    // A passing step that prints the words (a test about disk-full handling) is a pass.
    let passes = run_in(
        &env,
        &work,
        "echo 'handled: No space left on device'; echo '2 passed in 0.01s'",
    )
    .await;
    assert!(!not_run(&passes), "{passes:?}");
    assert_eq!(passes.exit_code, Some(0));
    // A step whose runner reported failing tests is red, whatever else it printed.
    let red = run_in(
        &env,
        &work,
        "echo 'OSError: [Errno 28] No space left on device'; echo '1 passed, 2 failed in 0.01s'; exit 1",
    )
    .await;
    assert!(!not_run(&red), "{red:?}");
    assert_eq!(red.exit_code, Some(1));
    assert_eq!(
        red.counts,
        Some(TestCounts {
            passed: 1,
            failed: 2,
            skipped: 0
        })
    );
    // A step stopped at its limit has no exit code to judge.
    let cwd = work.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let slow = run_step(
        &env,
        &step("echo 'No space left on device'; sleep 30"),
        &cwd,
        Duration::from_secs(1),
        4096,
    )
    .await;
    assert!(slow.timed_out);
    assert!(!not_run(&slow), "{slow:?}");
}

#[tokio::test]
async fn a_run_out_of_disk_after_passing_tests_is_not_run_and_drops_the_partial_counts() {
    let work = common::scratch("report-disk-partial");
    let env = env_for(&work);
    let s = run_in(
        &env,
        &work,
        "echo '3 passed in 0.01s'; echo 'write failed: No space left on device' 1>&2; exit 2",
    )
    .await;
    assert!(not_run(&s), "{s:?}");
    assert_eq!(s.exit_code, Some(2));
    assert_eq!(s.counts, None, "an incomplete run measures nothing");
}

// ---- the line that says the command was chosen from the manifest ----------------------------------------------------------------

use agentd::direct::report::run::set_plan_note;
use harness_protocol::direct::TestStep;

const NOTE: &str = "PLAN NOTE: cargo workspace, 14 members";

fn cargo_step(source: TestCommandSource, tail: &str) -> TestStep {
    TestStep {
        role: TestStepRole::Test,
        source,
        command: "cargo test".to_string(),
        exit_code: Some(101),
        timed_out: false,
        duration_ms: 10,
        limit_s: Some(300),
        counts: None,
        counts_from: None,
        output_tail: tail.to_string(),
        output_truncated: false,
        logged: None,
    }
}

#[test]
fn the_line_goes_to_the_head_of_the_output_of_a_cargo_test_chosen_from_the_manifest() {
    let mut s = cargo_step(TestCommandSource::Detected, "compiling a\nerror: boom\n");
    set_plan_note(&mut s, Some(14), 4096);
    assert_eq!(s.output_tail, format!("{NOTE}\ncompiling a\nerror: boom\n"));
    assert_eq!(s.plan_note_members(), Some(14));
    assert!(!s.output_truncated);
}

#[test]
fn the_line_comes_after_the_not_run_line_so_the_reason_stays_the_first_line() {
    let mut s = cargo_step(
        TestCommandSource::Detected,
        "NOT RUN: cargo is not installed here\nsh: 1: cargo: not found\n",
    );
    s.counts_from = Some("not run".to_string());
    set_plan_note(&mut s, Some(14), 4096);
    assert_eq!(
        s.output_tail,
        format!("NOT RUN: cargo is not installed here\n{NOTE}\nsh: 1: cargo: not found\n")
    );
    assert_eq!(s.not_run_reason(), Some("cargo is not installed here"));
    assert_eq!(s.plan_note_members(), Some(14));
    // A step agentd did not start has no newline after its reason.
    let mut bare = TestStep::not_run(
        TestStepRole::Test,
        TestCommandSource::Detected,
        "cargo test",
        "the session has no container runtime",
        None,
    );
    set_plan_note(&mut bare, Some(14), 4096);
    assert_eq!(
        bare.output_tail,
        format!("NOT RUN: the session has no container runtime\n{NOTE}\n")
    );
    assert_eq!(bare.plan_note_members(), Some(14));
}

#[test]
fn the_output_gives_way_to_the_line_at_the_tail_cap_from_its_front() {
    let body = format!("{}\nthe end\n", "x".repeat(200));
    let mut s = cargo_step(TestCommandSource::Detected, &body);
    s.output_truncated = false;
    set_plan_note(&mut s, Some(14), 120);
    assert!(s.output_tail.len() <= 120, "{}", s.output_tail.len());
    assert!(s.output_tail.starts_with(&format!("{NOTE}\n")));
    assert!(s.output_tail.ends_with("the end\n"));
    assert!(s.output_truncated);
    assert_eq!(s.plan_note_members(), Some(14));
}

#[test]
fn a_step_of_another_kind_is_left_as_it_is() {
    for (source, command, role) in [
        (
            TestCommandSource::TestCommandFile,
            "cargo test",
            TestStepRole::Test,
        ),
        (
            TestCommandSource::GoalCard,
            "cargo test",
            TestStepRole::Test,
        ),
        (
            TestCommandSource::Detected,
            "cargo test --workspace",
            TestStepRole::Test,
        ),
        (
            TestCommandSource::Detected,
            "go test -v ./...",
            TestStepRole::Test,
        ),
        (
            TestCommandSource::Detected,
            "cargo test",
            TestStepRole::Setup,
        ),
    ] {
        let mut s = cargo_step(source, "output\n");
        s.command = command.to_string();
        s.role = role;
        set_plan_note(&mut s, Some(14), 4096);
        assert_eq!(s.output_tail, "output\n", "{source:?} {command} {role:?}");
    }
}

#[test]
fn a_line_the_repository_printed_at_the_head_is_removed_and_never_taken_for_agentds() {
    // Without a note: the repository's own line goes.
    let forged = format!(
        "{}\nPLAN NOTE: cargo workspace, 7 members\nreal output\n",
        NOTE
    );
    let mut s = cargo_step(TestCommandSource::Detected, &forged);
    set_plan_note(&mut s, None, 4096);
    assert_eq!(s.output_tail, "real output\n");
    assert_eq!(s.plan_note_members(), None);
    // With a note: exactly one line is left, agentd's own.
    let mut t = cargo_step(
        TestCommandSource::Detected,
        "PLAN NOTE: cargo workspace, 99 members\nreal output\n",
    );
    set_plan_note(&mut t, Some(14), 4096);
    assert_eq!(t.output_tail, format!("{NOTE}\nreal output\n"));
    // And after a NOT RUN line.
    let mut n = cargo_step(
        TestCommandSource::Detected,
        "NOT RUN: cargo is not installed here\nPLAN NOTE: cargo workspace, 99 members\nsh: 1: cargo: not found\n",
    );
    n.counts_from = Some("not run".to_string());
    set_plan_note(&mut n, None, 4096);
    assert_eq!(
        n.output_tail,
        "NOT RUN: cargo is not installed here\nsh: 1: cargo: not found\n"
    );
}

#[test]
fn no_note_and_no_forged_line_leave_the_step_untouched() {
    let mut s = cargo_step(TestCommandSource::Detected, "plain output\n");
    let before = s.clone();
    set_plan_note(&mut s, None, 4096);
    assert_eq!(s, before);
}
