//! The reach of the run's secret set. A secret that the engine prints in the goal-complete summary does not reach an event; the longest
//! secret is replaced first; the digest's source structure prints no secret when it is formatted with Debug.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::classify::redact_known;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::input::DigestSources;
use agentd::secrets::SecretSet;
use common::{CANARY_SECRET, plugin_folder};
use direct_support::{Direct, Opts, no_secrets};
use std::collections::BTreeMap;
use std::sync::Arc;

fn goal_settings() -> GoalSettings {
    let m: BTreeMap<String, String> = [
        (
            "AGENTD_GOAL_PLUGIN_DIR",
            plugin_folder().display().to_string(),
        ),
        ("AGENTD_GOAL_POLL_MS", "20".to_string()),
        ("AGENTD_GOAL_SETTLE_MS", "150".to_string()),
        ("AGENTD_GOAL_DEAD_GRACE_MS", "150".to_string()),
        ("AGENTD_GOAL_KICK_AFTER_S", "1".to_string()),
        ("AGENTD_GOAL_DRAIN_S", "3".to_string()),
        ("AGENTD_GOAL_CLEARED_CONFIRM_MS", "200".to_string()),
        ("AGENTD_GOAL_FALLBACK_QUIET_S", "1".to_string()),
        ("AGENTD_GOAL_CLAIM_WAIT_MS", "300".to_string()),
        ("AGENTD_GOAL_EVENTS_EVERY_S", "0".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

#[tokio::test(flavor = "multi_thread")]
async fn b592_a_secret_in_the_goal_complete_summary_does_not_reach_the_goal_event() {
    let d = Direct::start(Opts {
        scenario: "goal_loop_prints_secret",
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    let goal = GoalHook::for_engine("cline", goal_settings()).expect("the plugin folder is there");
    let modules: Vec<Arc<dyn DirectHook>> = vec![goal];
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let events: Vec<_> = d
        .aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    // TRIGGER PRESENT: the engine did print the secret into its done claim, and the loop did end on that claim.
    let done = events
        .iter()
        .find(|v| v["phase"] == "done")
        .unwrap_or_else(|| panic!("no done event: {events:?}\n{}", d.engine_trace()));
    let all = format!("{events:?}");
    assert!(
        done["summary"]
            .as_str()
            .is_some_and(|s| s.starts_with("all done")),
        "{done}"
    );
    assert!(
        !all.contains(CANARY_SECRET),
        "the goal events carry a secret: {all}"
    );
    no_secrets(&all);
    assert!(
        done["summary"].as_str().unwrap().contains("<redacted>"),
        "the secret is replaced, not cut: {done}"
    );
    assert!(
        done["text"].as_str().unwrap().contains("<redacted>"),
        "the sentence is replaced too: {done}"
    );
    no_secrets(&d.log.lines().join("\n"));
}

#[test]
fn b592_the_longest_secret_is_replaced_first() {
    let short = "secret-prefix-1234".to_string();
    let long = "secret-prefix-1234-and-its-tail-5678".to_string();
    let text = format!("a {long} b {short} c");
    // The shorter value is listed first on purpose: replaced in that order, it would leave "-and-its-tail-5678" behind.
    let out = redact_known(&text, &[short.clone(), long.clone()]);
    assert_eq!(out, "a <redacted> b <redacted> c");
    let set = SecretSet::new(vec![short, long]);
    assert_eq!(set.redact(&text), "a <redacted> b <redacted> c");
}

#[test]
fn b592_digest_sources_print_no_secret_with_debug() {
    let sources = DigestSources {
        task: "do the work".to_string(),
        secrets: vec![
            CANARY_SECRET.to_string(),
            "another-secret-value".to_string(),
        ],
        ..DigestSources::default()
    };
    let shown = format!("{sources:?} {sources:#?}");
    assert!(!shown.contains(CANARY_SECRET), "{shown}");
    assert!(!shown.contains("another-secret-value"), "{shown}");
    assert!(
        shown.contains("do the work"),
        "the other members still print: {shown}"
    );
    assert!(shown.contains("2 value"), "the count prints: {shown}");
}
