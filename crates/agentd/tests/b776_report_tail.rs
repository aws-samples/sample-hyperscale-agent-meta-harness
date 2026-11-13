//! The clean-checkout test report keeps the output of a step whole up to a stage setting (`AGENTD_REPORT_TAIL_KIB`, 256 KiB by
//! default), not the last 4 KiB. The time one command may run reaches the session bound.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::report::{ReportHook, Settings};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{
    DEFAULT_REPORT_TAIL_BYTES, MAX_REPORT_STEP_S, MAX_REPORT_TAIL_BYTES, TestReport, s3 as keys,
};
use std::sync::Arc;
use std::time::Duration;

fn stage(tail: usize) -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail,
    }
}

async fn report_with(settings: Settings) -> TestReport {
    let d = Direct::start(Opts {
        scenario: "report_loud",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let hooks = Hooks::new(vec![
        Arc::new(ReportHook::new(settings)) as Arc<dyn DirectHook>
    ]);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let bytes = d
        .aws
        .get(&keys::test_report_key(RUN))
        .expect("a report was uploaded");
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn b776_a_step_that_printed_over_100_kib_keeps_all_of_it_by_default() {
    let r = report_with(stage(DEFAULT_REPORT_TAIL_BYTES)).await;
    assert_eq!(r.steps.len(), 1, "{r:?}");
    let s = &r.steps[0];
    assert_eq!(s.exit_code, Some(0));
    assert!(
        s.output_tail.len() > 100_000,
        "{} bytes",
        s.output_tail.len()
    );
    assert!(
        s.output_tail.starts_with("1\n2\n3\n"),
        "the first line is there: {:?}",
        &s.output_tail[..20]
    );
    assert!(s.output_tail.ends_with("19999\n20000\n"));
    assert!(!s.output_truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn b776_the_tail_setting_keeps_only_the_end_when_the_stage_asks_for_less() {
    let r = report_with(stage(8 * 1024)).await;
    let s = &r.steps[0];
    assert!(
        s.output_tail.len() <= 8 * 1024,
        "{} bytes",
        s.output_tail.len()
    );
    assert!(
        s.output_tail.len() > 4 * 1024,
        "{} bytes",
        s.output_tail.len()
    );
    assert!(s.output_tail.ends_with("19999\n20000\n"));
    assert!(s.output_truncated, "the report says output was cut");
}

#[test]
fn b776_the_tail_and_step_settings_come_from_the_environment() {
    let none = |_: &str| None;
    assert_eq!(Settings::from_vars(&none).tail, DEFAULT_REPORT_TAIL_BYTES);
    let get = |k: &str| (k == "AGENTD_REPORT_TAIL_KIB").then(|| "1024".to_string());
    assert_eq!(Settings::from_vars(&get).tail, 1024 * 1024);
    let top = format!("{}", MAX_REPORT_TAIL_BYTES / 1024);
    let get = |k: &str| (k == "AGENTD_REPORT_TAIL_KIB").then(|| top.clone());
    assert_eq!(Settings::from_vars(&get).tail, MAX_REPORT_TAIL_BYTES);
    // A value that does not parse or lies outside 4 KiB to the ceiling is the default.
    for bad in ["", "0", "3", "abc", "-5", "99999999"] {
        let g = |k: &str| (k == "AGENTD_REPORT_TAIL_KIB").then(|| bad.to_string());
        assert_eq!(
            Settings::from_vars(&g).tail,
            DEFAULT_REPORT_TAIL_BYTES,
            "{bad:?}"
        );
    }
    // The time of one command and of the report go up to the session bound.
    let long = format!("{MAX_REPORT_STEP_S}");
    let g = |k: &str| {
        (k == "AGENTD_REPORT_STEP_S" || k == "AGENTD_REPORT_BUDGET_S").then(|| long.clone())
    };
    let s = Settings::from_vars(&g);
    assert_eq!(s.step, Duration::from_secs(u64::from(MAX_REPORT_STEP_S)));
    assert_eq!(s.budget, Duration::from_secs(u64::from(MAX_REPORT_STEP_S)));
    let over = format!("{}", MAX_REPORT_STEP_S + 1);
    let g = |k: &str| (k == "AGENTD_REPORT_STEP_S").then(|| over.clone());
    assert_eq!(
        Settings::from_vars(&g).step,
        Duration::from_secs(300),
        "over the bound is the default"
    );
}
