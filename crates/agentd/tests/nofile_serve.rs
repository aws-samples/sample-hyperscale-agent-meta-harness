//! The open-files cap on the real `agentd serve` binary: it lowers its soft limit once at its start, logs one line, and the engine it
//! launches (the fake engine's wrapper records `ulimit` into its HOME) and the command of the report step (the scenario
//! `report_prints_nofile` leaves a `.ham/test-command` that prints the two limits) both see the cap. The test process raises its own soft limit
//! to a high start value and never lowers it, so the tests of this file may run in one process.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::nofile::{DEFAULT_CAP, VAR};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{TestReport, s3 as keys};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// A high soft limit for the processes this test starts, as a container with a huge default has; the value is the same in every test of the
/// file. Returns the soft and the hard limit the started processes inherit.
fn high_start() -> (u64, Option<u64>) {
    let now = getrlimit(Resource::Nofile);
    let start = now.maximum.map_or(1_048_576, |h| h.min(1_048_576));
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(start),
            maximum: now.maximum,
        },
    )
    .unwrap();
    assert!(
        start >= 1_024,
        "the test needs a hard limit of at least 1024; this host allows {start}"
    );
    (start, now.maximum)
}

fn shown(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string())
}

async fn run(env: Vec<(&'static str, String)>) -> (Direct, String, TestReport) {
    let d = Direct::start_process(Opts {
        scenario: "report_prints_nofile",
        engine_env: vec![("WRAP_RECORD_NOFILE", "1".to_string())],
        agentd_env: env,
        timeout_s: 600,
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    d.wait_healthy().await;
    let seen = std::fs::read_to_string(d.agent_data().join("home/nofile-seen"))
        .expect("the engine wrapper records its limits");
    let report: TestReport = serde_json::from_slice(
        &d.aws
            .get(&keys::test_report_key(RUN))
            .expect("a test report"),
    )
    .unwrap();
    (d, seen, report)
}

fn log_of(d: &Direct) -> String {
    std::fs::read_to_string(&d.process.as_ref().unwrap().log_file).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn agentd_serve_lowers_its_limit_and_the_engine_and_the_report_step_see_the_cap() {
    let (start, hard) = high_start();
    let cap = 512;
    assert!(start > cap, "the trigger is a soft limit above the cap");
    let (d, seen, report) = run(vec![(VAR, cap.to_string())]).await;

    // The engine's process.
    assert_eq!(
        seen,
        format!("{cap}\n{}\n", shown(hard)),
        "the engine sees the cap and the hard limit as they are"
    );

    // The report step's command: the two limits, then the pass line.
    assert_eq!(report.steps.len(), 1, "{report:?}");
    assert_eq!(report.steps[0].exit_code, Some(0), "{report:?}");
    assert_eq!(
        report.steps[0].output_tail,
        format!("{cap}\n{}\n2 passed in 0.01s\n", shown(hard)),
        "the report step's command sees the cap and the hard limit as they are"
    );

    // One line in the runtime log says what was done, before the server listens.
    let log = log_of(&d);
    let lowered: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("agentd: open files limit"))
        .collect();
    assert_eq!(lowered.len(), 1, "{log}");
    assert!(
        lowered[0].starts_with("agentd: open files limit lowered"),
        "{log}"
    );
    assert!(
        lowered[0].contains(&start.to_string()) && lowered[0].contains(&cap.to_string()),
        "{log}"
    );
    let at = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing in {log}"))
    };
    assert!(at("open files limit lowered") < at("listening on"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_setting_the_default_cap_applies() {
    let (start, hard) = high_start();
    let (d, seen, _) = run(vec![]).await;
    let expect = start.min(DEFAULT_CAP);
    assert_eq!(
        seen,
        format!("{expect}\n{}\n", shown(hard)),
        "{}",
        log_of(&d)
    );
    let log = log_of(&d);
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("agentd: open files limit"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    let word = if start > DEFAULT_CAP {
        "lowered"
    } else {
        "left alone"
    };
    assert!(
        lines[0].starts_with(&format!("agentd: open files limit {word}")),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_switch_off_leaves_the_limit_as_the_session_has_it() {
    let (start, hard) = high_start();
    for off in ["off", "0"] {
        let (d, seen, _) = run(vec![(VAR, off.to_string())]).await;
        assert_eq!(
            seen,
            format!("{start}\n{}\n", shown(hard)),
            "{off}: {}",
            log_of(&d)
        );
        let log = log_of(&d);
        assert!(
            log.contains("agentd: open files limit left alone") && log.contains(VAR),
            "{off}: {log}"
        );
        assert!(!log.contains("open files limit lowered"), "{off}: {log}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_value_is_a_config_error_line_and_the_default_applies() {
    let (start, hard) = high_start();
    let (d, seen, _) = run(vec![(VAR, "lots".to_string())]).await;
    let log = log_of(&d);
    assert_eq!(
        seen,
        format!("{}\n{}\n", start.min(DEFAULT_CAP), shown(hard)),
        "{log}"
    );
    assert!(
        log.lines()
            .any(|l| l.starts_with("agentd: ") && l.contains(VAR) && l.contains("lots")),
        "{log}"
    );
}
