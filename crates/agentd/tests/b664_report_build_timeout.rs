//! In the worker: a test command that is stopped at its limit while its output still shows only build lines is reported as building, not as
//! a test run that took too long. The whole agent runs in process against the fake AWS and the fake ACP engine; the scenario `report_slow_build`
//! writes a `.ham/test-command` that prints two `Compiling` lines and then sleeps past its limit. That command is a shell line and no runner the
//! reading knows, so the sentence says that the output shows build work and makes no claim that no test began (the review of the 5.15 head); a
//! stand-in `cargo`, a runner that prints a line when its tests begin, gets the sentence that says so.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks, Limits};
use agentd::direct::report::{ReportHook, Settings};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{TestReport, TestReportStatus, s3 as keys};
use std::sync::Arc;
use std::time::Duration;

fn settings(budget_s: u64, step_s: u64) -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(budget_s),
        step: Duration::from_secs(step_s),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

async fn run_with(d: &Direct, s: Settings, limits: Limits) {
    let hooks =
        Hooks::new(vec![Arc::new(ReportHook::new(s)) as Arc<dyn DirectHook>]).with_limits(limits);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
}

fn report_of(d: &Direct) -> Option<TestReport> {
    let bytes = d.aws.get(&keys::test_report_key(RUN))?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn b664_a_test_command_stopped_while_it_still_printed_build_lines_is_reported_as_building() {
    let d = Direct::start(Opts {
        scenario: "report_slow_build",
        report_step_s: Some(2),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(40, 30), Limits::default()).await;
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::TimedOut, "{r:?}");
    assert!(r.steps[0].timed_out, "{r:?}");
    assert!(
        r.steps[0].output_tail.contains("Compiling b v0.1.0"),
        "{r:?}"
    );
    let s = r.summary();
    assert!(s.stopped_in_build_work(), "{s:?}");
    assert!(!s.stopped_while_building(), "{s:?}");
    assert!(
        s.sentence().ends_with(
            "was stopped at its limit of 2 s; the end of its output shows build work, and the report cannot tell whether a test began, so it says nothing about the tests."
        ),
        "{}",
        s.sentence()
    );
    assert_eq!(s.claim(), None);
    assert_eq!(s.counts, None);
}

/// A stand-in `cargo` that prints two build lines and outlives its limit: `cargo test` is a runner whose first test line the reading knows, so
/// the step that was stopped says that no test had begun.
#[tokio::test(flavor = "multi_thread")]
async fn fix515_b664_a_cargo_command_stopped_in_its_build_says_no_test_had_begun() {
    use agentd::direct::report::plan::PlannedStep;
    use agentd::direct::report::run::{RunEnv, run_step};
    use harness_protocol::direct::{TestCommandSource, TestStepRole};
    use std::os::unix::fs::PermissionsExt;

    let bin = common::scratch("fix515-cargo-bin");
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\nprintf '   Compiling a v0.1.0\\n   Compiling b v0.1.0\\n'\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = RunEnv {
        sandbox: None,
        work: common::scratch("fix515-cargo-work"),
        run_as: None,
        path: vec![bin, "/usr/bin".into(), "/bin".into()],
        engine_env: Default::default(),
        engine_home: None,
        cargo_config: None,
    };
    let planned = PlannedStep {
        role: TestStepRole::Test,
        source: TestCommandSource::Detected,
        command: "cargo test".into(),
        logged: None,
    };
    let cwd = common::scratch("fix515-cargo-tree");
    let mut step = run_step(&env, &planned, &cwd, Duration::from_secs(2), 4096).await;
    assert!(step.timed_out, "{step:?}");
    step.limit_s = Some(2);
    let r = TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: "a".repeat(40),
        status: TestReportStatus::TimedOut,
        reason: Some("a command was stopped at the report time limit".into()),
        started_at: None,
        duration_ms: step.duration_ms,
        steps: vec![step],
        summary: None,
    };
    let s = r.summary();
    assert!(s.stopped_while_building(), "{s:?}");
    assert_eq!(
        s.sentence(),
        "`cargo test` was stopped at its limit of 2 s while it was still building: its output shows build work and no test that began, so this report says nothing about the tests."
    );
}
