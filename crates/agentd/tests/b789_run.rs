//! The engine-neutral layer in a whole direct run: the neutral config-root variable is in the launch of the engine, and an engine with no
//! listing command has its rules checked on the disk while Cline keeps its own listing preflight.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts};
use harness_protocol::runtime::TurnOutcome;

#[tokio::test(flavor = "multi_thread")]
async fn b789_the_launch_of_the_qualified_engine_carries_the_config_root_variable_and_it_is_the_directory_the_older_variable_pointed_into()
 {
    let d = Direct::start(Opts::default()).await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let probe = std::fs::read_to_string(d.agent_data().join("home/engine-probe.txt")).unwrap();
    let env: std::collections::BTreeMap<&str, &str> = probe
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("--- "))
        .filter_map(|l| l.split_once('='))
        .collect();
    let config = d.agent_data().join("config");
    assert_eq!(
        env.get("HAMH_ENGINE_CONFIG_ROOT").copied(),
        Some(config.to_str().unwrap()),
        "the root of the harness's files is the config directory for Cline"
    );
    // `${CLINE_GLOBAL_SETTINGS_PATH%/locked/*}`, the older way to the same directory.
    let old = env.get("CLINE_GLOBAL_SETTINGS_PATH").unwrap();
    assert_eq!(
        old.rsplit_once("/locked/").unwrap().0,
        config.to_str().unwrap()
    );
    // The kill switch of the qualified engine reached the launch (the run was not refused for lacking it).
    assert_eq!(env.get("CLINE_NO_AUTO_UPDATE"), Some(&"1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn b789_cline_keeps_its_listing_preflight_and_an_engine_with_no_listing_is_checked_on_the_disk()
 {
    let cline = Direct::start(Opts::default()).await;
    let (_, resp) = cline.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        !cline
            .log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn rules placement verified on the disk")),
        "Cline's check is its own listing: {:#?}",
        cline.log.lines()
    );

    let fake = Direct::start(Opts {
        engine: "fake-acp",
        ..Opts::default()
    })
    .await;
    let (_, resp) = fake.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let lines = fake.log.lines();
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no log line starting {prefix:?} in {lines:#?}"))
    };
    let placed = at("direct turn rules placed:");
    let protected = at("direct turn config directory protected:");
    let verified = at("direct turn rules placement verified on the disk: 1 file(s)");
    let starting = at("direct turn engine starting");
    assert!(
        placed < protected && protected < verified && verified < starting,
        "{placed} {protected} {verified} {starting}"
    );
}
