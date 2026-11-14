//! The clean-checkout report plans the repository's own test scripts and starts the services the repository declares.
//!
//! The six delivered trees of the release gate of 2026-11-01 (web, events and enterprise, two each) are replayed offline: their Compose file, the
//! names of their files and the commands of their lifecycle log are in `b600_trees/`. Their test scripts are stand-ins made here: a unit script
//! that exits 0 and prints counts, and an integration or end-to-end script that prints the refusal a database or broker that is not there gives
//! and exits 1. The services are a fake `ham-services` in a folder of the test. Nothing here starts a real server.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::report::note_needs;
use agentd::direct::report::parse::refused_port;
use agentd::direct::report::plan::{Plan, PlannedStep, plan};
use agentd::direct::report::run::{RunEnv, run_step_scanned};
use agentd::direct::report::services::{self, Kind, Live};
use harness_protocol::direct::{
    BANNED_CLAIM_WORDS, TestCounts, TestReport, TestReportStatus, TestStep, TestStepRole,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TREES: [(&str, &str); 6] = [
    ("web1", include_str!("b600_trees/web1.json")),
    ("web2", include_str!("b600_trees/web2.json")),
    ("events1", include_str!("b600_trees/events1.json")),
    ("events2", include_str!("b600_trees/events2.json")),
    ("ent1", include_str!("b600_trees/ent1.json")),
    ("ent2", include_str!("b600_trees/ent2.json")),
];

const REFUSED_PG: &str = "psycopg.OperationalError: connection to server at \"127.0.0.1\", port 5432 failed: Connection refused";
const REFUSED_MQ: &str = "dial tcp 127.0.0.1:5672: connect: connection refused";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn executable(path: &Path, text: &str) {
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The content of a stand-in test script by its name.
fn script_text(name: &str, refusals: &[&str]) -> String {
    if name.contains("unit") {
        "#!/bin/sh\necho \"42 passed in 0.30s\"\nexit 0\n".to_string()
    } else {
        let lines: String = refusals.iter().map(|r| format!("echo '{r}'\n")).collect();
        format!("#!/bin/sh\n{lines}echo \"6 failed in 0.41s\"\nexit 1\n")
    }
}

/// The tree of a recorded run in a fresh directory.
fn tree_of(name: &str, src: &str, refusals: &[&str]) -> PathBuf {
    let v: Value = serde_json::from_str(src).unwrap();
    let root = common::scratch(&format!("b600-{name}")).join("tree");
    std::fs::create_dir_all(&root).unwrap();
    for f in v["files"].as_array().unwrap() {
        let f = f.as_str().unwrap();
        if f.ends_with(".sh") {
            let base = f.rsplit('/').next().unwrap();
            executable(&root.join(f), &script_text(base, refusals));
        } else {
            write(&root.join(f), "");
        }
    }
    write(&root.join("compose.yaml"), v["compose"].as_str().unwrap());
    let log: Vec<&str> = v["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    write(
        &root.join(".ham/lifecycle/LOG.md"),
        &format!("# Observed runs\n{}\n", log.join("\n")),
    );
    let goal: Vec<&str> = v["goal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    write(
        &root.join(".ham/lifecycle/GOAL.md"),
        &format!("# Goal\n{}\n", goal.join("\n")),
    );
    root
}

fn commands(p: Plan) -> Vec<String> {
    match p {
        Plan::Steps(s) => s
            .into_iter()
            .filter(|s| s.role == TestStepRole::Test)
            .map(|s| s.command)
            .collect(),
        Plan::NoCommand(why) => panic!("no command: {why}"),
    }
}

fn env_for(work: &Path, bin: Option<&Path>) -> RunEnv {
    let mut path = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
    if let Some(b) = bin {
        path.insert(0, b.to_path_buf());
    }
    RunEnv {
        sandbox: None,
        work: work.to_path_buf(),
        run_as: None,
        path,
        engine_env: BTreeMap::new(),
        engine_home: None,
        cargo_config: None,
    }
}

/// A password made at run time, so that no password-shaped literal is committed.
fn stand_in_password() -> String {
    ["0123456789abcdef", "0123456789abcdef"].concat()
}

/// A folder with a fake `ham-services`: `postgres` and `rabbitmq` end with `code` (and print `why` when they fail), `env` prints the variables
/// of a running pair, and `stop` leaves a file named `stopped`.
fn fake_services(code: i32, why: &str) -> PathBuf {
    let pw = stand_in_password();
    let bin = common::scratch("b600-bin");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  postgres|rabbitmq) if [ {code} -ne 0 ]; then echo \"{why}\" >&2; exit {code}; fi; echo \"ham-services: $1 ready\" ;;\n  env) echo \"export DATABASE_URL='postgres://app@127.0.0.1:5432/app?sslmode=disable'\"; echo \"export BROKER_PASSWORD='{pw}'\"; echo 'export BROKER_URL=\"amqp://app:${{BROKER_PASSWORD}}@127.0.0.1:5672/\"'; echo \"export PATH='/evil'\" ;;\n  stop) touch \"{dir}/stopped\" ;;\nesac\n",
        dir = bin.display()
    );
    executable(&bin.join("ham-services"), &script);
    bin
}

async fn run_all(
    env: &RunEnv,
    root: &Path,
    steps: &[PlannedStep],
    live: &[Live],
    up: &dyn Fn(u16) -> bool,
) -> Vec<TestStep> {
    let mut out = Vec::new();
    for p in steps {
        let (mut s, refused) = run_step_scanned(env, p, root, Duration::from_secs(30), 4096).await;
        note_needs(&mut s, p.role, &refused, live, up, 4096);
        out.push(s);
    }
    out
}

fn report(steps: Vec<TestStep>) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: "R1".into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 1,
        steps,
        summary: None,
    }
}

// ---- the plan --------------------------------------------------------------------------------------------------------------

#[test]
fn b729_a_log_that_holds_a_python_venv_prefix_is_not_replayed() {
    let root = common::scratch("b729-venv").join("tree");
    for s in ["test-e2e.sh", "test-all.sh"] {
        executable(&root.join("scripts").join(s), "#!/bin/sh\nexit 0\n");
    }
    write(
        &root.join(".ham/lifecycle/LOG.md"),
        "# Observed runs\nRUN | PYTHON=.venv/bin/python bash scripts/test-e2e.sh | exit 127 | pass unknown fail unknown | x\nRUN | .venv/bin/python -m pytest api/tests -q | exit 127 | pass unknown fail unknown | x\nRUN | PATH=.venv/bin:$PATH bash scripts/test-e2e.sh | exit 127 | pass unknown fail unknown | x\n",
    );
    let got = commands(plan(&root));
    assert_eq!(got, ["bash scripts/test-e2e.sh"]);
    assert!(got.iter().all(|c| !c.contains(".venv")));
}

#[test]
fn b729_a_session_prefix_is_left_off_and_a_session_program_is_left_out_of_a_free_form_command() {
    let root = common::scratch("b729-free").join("tree");
    write(&root.join("pytest.ini"), "");
    write(
        &root.join(".ham/lifecycle/LOG.md"),
        "# Observed runs\nRUN | PYTHON=.venv/bin/python python3 -m pytest -q | exit 0 | pass 3 fail 0 | x\nRUN | .venv/bin/python -m pytest api -q | exit 127 | pass unknown fail unknown | x\nRUN | CI=1 python3 -m pytest -q tests | exit 0 | pass 3 fail 0 | x\n",
    );
    // `.venv/bin/python` exists in this tree for one of the two runs: a real file is not a session leftover.
    assert_eq!(
        commands(plan(&root)),
        ["python3 -m pytest -q", "CI=1 python3 -m pytest -q tests"]
    );
    write(&root.join(".venv/bin/python"), "");
    assert!(commands(plan(&root)).contains(&".venv/bin/python -m pytest api -q".to_string()));
}

#[test]
fn b729_a_prefix_that_names_a_file_the_command_writes_is_not_a_session_leftover() {
    let root = common::scratch("b729-out").join("tree");
    write(&root.join("pytest.ini"), "");
    write(
        &root.join(".ham/lifecycle/LOG.md"),
        "# Observed runs\nRUN | JUNIT_OUT=reports/junit.xml python3 -m pytest -q | exit 0 | pass 3 fail 0 | x\n",
    );
    assert_eq!(
        commands(plan(&root)),
        ["JUNIT_OUT=reports/junit.xml python3 -m pytest -q"]
    );
}

#[test]
fn b729_the_test_scripts_of_the_repository_come_before_the_free_form_commands_of_the_log() {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    assert_eq!(
        commands(plan(&root)),
        [
            "bash scripts/test-api-unit.sh",
            "bash scripts/test-ui-unit.sh",
            "bash scripts/test-integration.sh",
            "bash scripts/test-e2e.sh"
        ]
    );
}

#[test]
fn b729_the_six_recorded_runs_plan_scripts_of_their_trees_and_no_session_path() {
    for (name, src) in TREES {
        let root = tree_of(name, src, &[REFUSED_PG]);
        let got = commands(plan(&root));
        assert!(!got.is_empty(), "{name}");
        for c in &got {
            assert!(
                !c.contains(".venv") && !c.contains("PYTHON="),
                "{name}: {c}"
            );
            assert!(c.starts_with("bash scripts/test-"), "{name}: {c}");
        }
    }
}

#[test]
fn b729_a_test_all_script_stands_for_the_others_and_a_repository_without_scripts_keeps_its_plan() {
    let (name, src) = TREES[2];
    let root = tree_of(name, src, &[REFUSED_PG]);
    let got = commands(plan(&root));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].ends_with("test-all.sh"), "{got:?}");
    // No script of the tree in the log: the plan is the log's last commands, as before.
    let bare = common::scratch("b729-bare").join("tree");
    write(&bare.join("pytest.ini"), "");
    write(
        &bare.join(".ham/lifecycle/LOG.md"),
        "# Observed runs\nRUN | python3 -m pytest -q | exit 0 | pass 3 fail 0 | x\n",
    );
    assert_eq!(commands(plan(&bare)), ["python3 -m pytest -q"]);
}

#[test]
fn b729_the_command_file_of_the_project_still_wins() {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    write(
        &root.join(".ham/test-command"),
        "bash scripts/test-api-unit.sh\n",
    );
    assert_eq!(commands(plan(&root)), ["bash scripts/test-api-unit.sh"]);
}

// ---- the services -------------------------------------------------------------------------------------------------------

#[test]
fn b600_the_compose_file_is_read_for_postgres_and_rabbitmq_images_by_name() {
    let text = "services:\n  db:\n    image: postgres:16\n    ports:\n      - \"127.0.0.1:5432:5432\"\n  mq:\n    image: \"docker.io/library/rabbitmq:3.13-management\" # broker\n  cache:\n    image: redis:7\n  app:\n    build: .\nvolumes:\n  data:\n";
    let got = services::parse_compose(text);
    assert_eq!(
        got,
        [
            ("db".to_string(), "postgres:16".to_string()),
            (
                "mq".to_string(),
                "docker.io/library/rabbitmq:3.13-management".to_string()
            ),
            ("cache".to_string(), "redis:7".to_string())
        ]
    );
    assert_eq!(services::kind_of_image("postgres:16"), Some(Kind::Postgres));
    assert_eq!(
        services::kind_of_image("docker.io/library/rabbitmq:3.13-management"),
        Some(Kind::Rabbitmq)
    );
    assert_eq!(services::kind_of_image("redis:7"), None);
    // A name that only contains the word is another image.
    assert_eq!(services::kind_of_image("mypostgres:1"), None);
    assert_eq!(services::kind_of_image("postgis/postgis:16"), None);
}

#[test]
fn b600_a_compose_file_that_is_empty_or_not_readable_declares_nothing() {
    for text in [
        "",
        "services: {}\n",
        "services:\n",
        "version: '3'\n",
        "services:\n\tdb:\n\t\timage: postgres:16\n",
        "x: [\n",
    ] {
        assert!(services::parse_compose(text).is_empty(), "{text:?}");
    }
    let root = common::scratch("b600-none").join("tree");
    write(
        &root.join("compose.yaml"),
        "services:\n  cache:\n    image: redis:7\n",
    );
    assert!(
        services::declared(&root).is_empty(),
        "a service with no local server is left alone"
    );
    assert!(services::declared(&common::scratch("b600-nofile")).is_empty());
}

#[tokio::test]
async fn b600_a_declared_service_is_started_as_a_setup_step_that_says_which_service_it_stands_in_for()
 {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    // The repository reads DATABASE_URL by name (its code is a stand-in here).
    write(
        &root.join("api/conftest.py"),
        "os.environ.get(\"DATABASE_URL\")\n",
    );
    let bin = fake_services(0, "");
    let work = common::scratch("b600-work");
    let declared = services::declared(&root);
    assert_eq!(declared.len(), 1);
    let started = services::start(
        &env_for(&work, Some(&bin)),
        &root,
        &declared,
        Instant::now(),
        Duration::from_secs(300),
        4096,
    )
    .await;
    assert_eq!(started.steps.len(), 1);
    let step = &started.steps[0];
    assert_eq!((step.role, step.exit_code), (TestStepRole::Setup, Some(0)));
    assert_eq!(step.command, "ham-services postgres");
    let line = step
        .stand_in()
        .expect("the step names what it stands in for");
    assert!(
        line.contains("compose service db (postgres:16 in compose.yaml)"),
        "{line}"
    );
    assert!(
        line.contains("local PostgreSQL server in its place"),
        "{line}"
    );
    assert!(line.contains("the Compose container did not run"), "{line}");
    assert!(started.live[0].started);
    // Only the variables the repository reads, and none of the others the helper printed.
    assert_eq!(started.vars.keys().collect::<Vec<_>>(), ["DATABASE_URL"]);
    // A repository that reads none of them gets none.
    let quiet = tree_of(name, src, &[REFUSED_PG]);
    let again = services::start(
        &env_for(&work, Some(&bin)),
        &quiet,
        &services::declared(&quiet),
        Instant::now(),
        Duration::from_secs(300),
        4096,
    )
    .await;
    assert!(again.vars.is_empty(), "{:?}", again.vars);
}

#[tokio::test]
async fn b600_a_service_that_cannot_start_is_a_step_not_run_with_the_reason_and_the_steps_still_run()
 {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    let bin = fake_services(1, "initdb failed; see the log");
    let work = common::scratch("b600-work");
    let started = services::start(
        &env_for(&work, Some(&bin)),
        &root,
        &services::declared(&root),
        Instant::now(),
        Duration::from_secs(300),
        4096,
    )
    .await;
    let step = &started.steps[0];
    assert!(step.is_not_run());
    assert!(
        step.not_run_reason().unwrap().contains("initdb failed"),
        "{}",
        step.output_tail
    );
    assert!(
        step.stand_in().unwrap().contains("could not be started"),
        "{}",
        step.output_tail
    );
    assert!(!started.live[0].started);
    assert!(started.vars.is_empty());
}

#[tokio::test]
async fn b600_a_missing_helper_is_said_to_be_missing() {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    let work = common::scratch("b600-work");
    let started = services::start(
        &env_for(&work, None),
        &root,
        &services::declared(&root),
        Instant::now(),
        Duration::from_secs(300),
        4096,
    )
    .await;
    let step = &started.steps[0];
    assert!(step.is_not_run());
    assert_eq!(
        step.not_run_reason(),
        Some("ham-services is not installed here"),
        "{}",
        step.output_tail
    );
}

#[tokio::test]
async fn b600_the_servers_are_stopped_after_the_steps() {
    let bin = fake_services(0, "");
    let work = common::scratch("b600-work");
    let root = common::scratch("b600-stop");
    services::stop(&env_for(&work, Some(&bin)), &root).await;
    assert!(bin.join("stopped").exists());
}

#[tokio::test]
async fn b600_the_six_recorded_runs_with_the_services_unavailable_keep_their_exit_codes_and_counts_and_name_the_need()
 {
    for (name, src) in TREES {
        let root = tree_of(name, src, &[REFUSED_PG, REFUSED_MQ]);
        let bin = fake_services(1, "ham-services: initdb failed");
        let work = common::scratch("b600-work");
        let env = env_for(&work, Some(&bin));
        let declared = services::declared(&root);
        let Plan::Steps(steps) = plan(&root) else {
            panic!("{name}")
        };
        let started = services::start(
            &env,
            &root,
            &declared,
            Instant::now(),
            Duration::from_secs(300),
            4096,
        )
        .await;
        let mut all = started.steps.clone();
        all.extend(run_all(&env, &root, &steps, &started.live, &|_| false).await);
        let r = report(all);
        let s = r.summary();
        // The enterprise trees declare no database of their own that the report knows; the others do.
        let failing: Vec<&TestStep> = r
            .steps
            .iter()
            .filter(|t| t.role == TestStepRole::Test && t.exit_code == Some(1))
            .collect();
        assert!(!failing.is_empty(), "{name}: the stand-in scripts fail");
        for f in &failing {
            // The raw exit code and the raw counts stay on the step.
            assert_eq!(f.exit_code, Some(1), "{name}");
            assert_eq!(
                f.counts,
                Some(TestCounts {
                    passed: 0,
                    failed: 6,
                    skipped: 0
                }),
                "{name}: {}",
                f.output_tail
            );
            assert!(!f.is_not_run(), "{name}: it ran");
            if !declared.is_empty() {
                assert!(!f.needs().is_empty(), "{name}: {}", f.output_tail);
            }
        }
        assert_eq!(s.claim(), None, "{name}");
        assert!(!s.all_exit_zero, "{name}");
        if !declared.is_empty() {
            assert!(
                s.sentence().contains("was not available"),
                "{name}: {}",
                s.sentence()
            );
            assert!(
                s.details.iter().any(|d| d.contains("it ran without")),
                "{name}: {:?}",
                s.details
            );
        }
        let _ = (BANNED_CLAIM_WORDS, s.sentence());
    }
}

#[tokio::test]
async fn b600_with_the_services_up_a_failing_step_is_a_failed_step_with_no_need() {
    let (name, src) = TREES[0];
    let root = tree_of(name, src, &[REFUSED_PG]);
    let bin = fake_services(0, "");
    let work = common::scratch("b600-work");
    let env = env_for(&work, Some(&bin));
    let started = services::start(
        &env,
        &root,
        &services::declared(&root),
        Instant::now(),
        Duration::from_secs(300),
        4096,
    )
    .await;
    let Plan::Steps(steps) = plan(&root) else {
        panic!()
    };
    // The probe says the port answers before and after: the output may say what it likes, the step stays a plain failure.
    let steps = run_all(&env, &root, &steps, &started.live, &|_| true).await;
    let integration = steps
        .iter()
        .find(|s| s.command.ends_with("test-integration.sh"))
        .unwrap();
    assert_eq!(integration.exit_code, Some(1));
    assert!(
        integration.needs().is_empty(),
        "{}",
        integration.output_tail
    );
    // The same server gone after the step is named, with that reason.
    let gone = run_all(
        &env,
        &root,
        &[PlannedStep {
            role: TestStepRole::Test,
            source: harness_protocol::direct::TestCommandSource::Detected,
            command: "bash scripts/test-integration.sh".into(),
            logged: None,
            install: None,
            fallback: None,
        }],
        &started.live,
        &|_| false,
    )
    .await;
    assert_eq!(
        gone[0].needs(),
        [("postgres", "it stopped answering during the step")]
    );
}

#[tokio::test]
async fn b600_a_step_that_passes_or_names_no_local_port_gets_no_need() {
    let root = common::scratch("b600-plain").join("tree");
    executable(
        &root.join("scripts/test-ok.sh"),
        "#!/bin/sh\necho \"3 passed in 0.1s\"\necho 'connection to 127.0.0.1:5432 refused'\nexit 0\n",
    );
    executable(
        &root.join("scripts/test-other.sh"),
        "#!/bin/sh\necho 'dial tcp 10.1.2.3:5432: connection refused'\necho \"1 failed in 0.1s\"\nexit 1\n",
    );
    executable(
        &root.join("scripts/test-mention.sh"),
        "#!/bin/sh\necho 'listening on 127.0.0.1:5432'\necho \"1 failed in 0.1s\"\nexit 1\n",
    );
    let work = common::scratch("b600-work");
    let env = env_for(&work, None);
    let live = [Live {
        kind: Kind::Postgres,
        started: false,
        reason: Some("x".into()),
    }];
    let mk = |c: &str| PlannedStep {
        role: TestStepRole::Test,
        source: harness_protocol::direct::TestCommandSource::Detected,
        command: c.into(),
        logged: None,
        install: None,
        fallback: None,
    };
    let got = run_all(
        &env,
        &root,
        &[
            mk("bash scripts/test-ok.sh"),
            mk("bash scripts/test-other.sh"),
            mk("bash scripts/test-mention.sh"),
        ],
        &live,
        &|_| false,
    )
    .await;
    for s in &got {
        assert!(s.needs().is_empty(), "{}: {}", s.command, s.output_tail);
    }
}

#[test]
fn b600_a_refusal_names_a_local_port_or_it_names_nothing() {
    assert_eq!(refused_port(REFUSED_PG), Some(5432));
    assert_eq!(refused_port(REFUSED_MQ), Some(5672));
    assert_eq!(
        refused_port(
            "org.postgresql.util.PSQLException: Connection to localhost:5432 refused. Check that the hostname and port are correct"
        ),
        Some(5432)
    );
    assert_eq!(
        refused_port("dial tcp 127.0.0.1:6379: connect: connection refused"),
        None
    );
    assert_eq!(
        refused_port("dial tcp db.internal:5432: connect: connection refused"),
        None
    );
    assert_eq!(refused_port("listening on 127.0.0.1:5432"), None);
    assert_eq!(
        refused_port("ModuleNotFoundError: No module named 'psycopg'"),
        None
    );
}

#[test]
fn b600_the_password_of_the_broker_is_not_kept_in_the_report() {
    let pw = stand_in_password();
    let vars = services::parse_exports(&format!(
        "export DATABASE_URL='postgres://app@127.0.0.1:5432/app?sslmode=disable'\nexport BROKER_PASSWORD='{pw}'\nexport BROKER_URL=\"amqp://app:${{BROKER_PASSWORD}}@127.0.0.1:5672/\"\nexport PATH='/evil'\nexport DATABASE_URL2='x'\nexport BROKER_URL3='$(touch x)'\n"
    ));
    assert_eq!(
        vars.keys().map(String::as_str).collect::<Vec<_>>(),
        ["BROKER_PASSWORD", "BROKER_URL", "DATABASE_URL"]
    );
    assert!(vars["BROKER_URL"].contains(&pw));
    let secrets = services::secrets(&vars);
    let mut step = TestStep::not_run(
        TestStepRole::Test,
        harness_protocol::direct::TestCommandSource::Detected,
        "x",
        "y",
        None,
    );
    step.output_tail = format!("failed to connect: {}", vars["BROKER_URL"]);
    step.command = format!("BROKER_PASSWORD={} bash x.sh", vars["BROKER_PASSWORD"]);
    services::scrub(&mut step, &secrets);
    assert!(!step.output_tail.contains(&pw) && !step.command.contains(&pw));
}
