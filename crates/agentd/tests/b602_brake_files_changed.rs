//! The brake event of the goal loop carries the count of changed files the harness measured at that turn end, so the card
//! of a run that changed no file can say nothing was delivered. The count is the one the summary records.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Hooks};
use common::plugin_folder;
use direct_support::{Direct, Opts, RUN};
use std::collections::BTreeMap;
use std::sync::Arc;

fn settings() -> GoalSettings {
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
async fn b602_the_brake_event_names_the_changed_files_the_summary_records() {
    let d = Direct::start(Opts {
        scenario: "goal_text_only_stall",
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    let goal =
        GoalHook::for_engine("cline", settings()).expect("the plugin folder is in the repository");
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let brake = d
        .aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["phase"] == "brake")
        .expect("the stall ends in a brake");
    let files = brake["filesChanged"]
        .as_u64()
        .unwrap_or_else(|| panic!("no filesChanged: {brake}"));
    assert_eq!(files, u64::from(d.summary().git.files_changed), "{brake}");
}

/// The second defect: a run had three end labels, one in each stop digest and another in the summary. The last stop digest of a run that the
/// goal loop's stopping rule ended carries the outcome and the label the summary records.
#[tokio::test(flavor = "multi_thread")]
async fn b602_the_last_stop_digest_carries_the_outcome_and_label_of_the_summary() {
    let d = Direct::start(Opts {
        scenario: "goal_text_only_stall",
        timeout_s: 60,
        ..Opts::default()
    })
    .await;
    let goal =
        GoalHook::for_engine("cline", settings()).expect("the plugin folder is in the repository");
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    let last = (1..=20)
        .filter_map(|n| {
            d.aws.get(&agentd::direct::audit_key(
                RUN,
                &format!("stop-digest-{n}.json"),
            ))
        })
        .next_back()
        .expect("the run wrote a stop digest");
    let digest: serde_json::Value = serde_json::from_slice(&last).unwrap();
    let s = d.summary();
    let text = digest.to_string();
    assert!(
        text.contains(&format!("\"outcome\":\"{}\"", s.outcome.as_str())),
        "summary {:?} / last digest {text}",
        s.outcome
    );
    assert!(
        text.contains(&serde_json::to_string(&s.label).unwrap()),
        "summary label {:?} / last digest {text}",
        s.label
    );
}
