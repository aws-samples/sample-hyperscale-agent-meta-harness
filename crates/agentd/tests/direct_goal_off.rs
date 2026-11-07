//! The goal loop and the request's `goal` member: a run whose start request says `goal: {"enabled": false}` is the plain run it was before the goal
//! plugin. The module is registered, so it decides at `on_startup`, from the request: it places nothing for the plugin, writes no goal, sets no plugin
//! mode, and the engine does not loop. It says so in one `goal-loop` event of phase `off`, which the card shows. The nudge, which is off unless the
//! user's `continue.md` asks for it, is off while the goal loop runs and is back exactly as it was when the loop is off: the loop sends its own
//! follow-up prompts, and the harness's bounded nudge on top of them would be two loops.
//!
//! Each test that proves "off" has its twin that proves the trigger: the same scenario and the same module without the member loops by itself.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Hooks};
use common::{plugin_folder, scratch};
use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::api::v1::ContinueSettings;
use harness_protocol::direct::{DirectNudge, DirectOutcome, s3 as keys};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A plugin folder like the image's (`common::plugin_folder`) and times short enough to run in seconds (the settings of `direct_goal.rs`).
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

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

async fn run_goal(d: &Direct) -> InvocationResponse {
    let goal =
        GoalHook::for_engine("cline", settings()).expect("the plugin folder is in the repository");
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

async fn start(scenario: &'static str, nudge: Option<ContinueSettings>) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 60,
        nudge,
        ..Opts::default()
    })
    .await
}

fn phases(d: &Direct) -> Vec<String> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v["phase"].as_str().unwrap().to_string())
        .collect()
}

fn rounds_started(d: &Direct) -> usize {
    std::fs::read_to_string(
        d.agent_data()
            .join("data/plugins/harness-rounds/rounds.jsonl"),
    )
    .unwrap_or_default()
    .lines()
    .filter(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| v["ev"] == "start"))
    .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_switches_the_goal_off_places_nothing_and_the_run_is_a_plain_run() {
    let mut d = start("goal_loop_passes", None).await;
    with_members(&mut d, json!({"goal": {"enabled": false}}));
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        s.cline_message.as_deref(),
        Some("Added percent_change with tests."),
        "the engine's first answer is the run's message: nothing looped"
    );
    // The run is what it was before the plugin: one prompt, no plugin, no goal file, no plugin mode.
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    assert!(!d.agent_data().join("config/plugins").exists());
    assert!(!d.agent_data().join("data/plugins").exists());
    assert!(!d.engine_probe().contains("CLINE_PLUGIN_MODE"));
    assert_eq!(rounds_started(&d), 0);
    // The card shows that it is off: one `off` event whose words say whose choice it was.
    assert_eq!(phases(&d), ["off"]);
    let (_, ev) = d.aws.progress("goal-loop").remove(0);
    let text = ev["text"].as_str().unwrap();
    assert!(
        text.starts_with("Goal loop off:") && text.contains("you switched it off"),
        "{text}"
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn goal loop off") && l.contains("you switched it off")),
        "{:?}",
        d.log.lines()
    );
    println!(
        "GOAL-OFF TRIGGER PRESENT: request goal.enabled=false rounds-started=0 plugin-placed=no prompts=1 event=off"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_run_without_the_member_loops_by_itself() {
    // The trigger of the case above: the module, the scenario and the plugin folder are the same; only the member is missing.
    let d = start("goal_loop_passes", None).await;
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(rounds_started(&d), 3, "the engine looped by itself");
    assert_eq!(
        d.summary().nudge,
        DirectNudge {
            requested: false,
            honored: false
        },
        "no nudge settings in the request: nothing to switch off"
    );
    assert!(d.agent_data().join("config/plugins").exists());
    let p = phases(&d);
    assert_eq!(p.first().map(String::as_str), Some("on"), "{p:?}");
    assert!(!p.iter().any(|x| x == "off"), "{p:?}");
    println!(
        "GOAL-OFF NEGATIVE PRESENT: no member rounds-started=3 plugin-placed=yes first-event=on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn goal_on_in_the_request_is_the_stage_default_and_the_loop_runs() {
    let mut d = start("goal_loop_passes", None).await;
    with_members(&mut d, json!({"goal": {"enabled": true}}));
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(rounds_started(&d), 3);
    assert!(!phases(&d).iter().any(|x| x == "off"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_goal_off_the_nudge_of_the_request_goes_out_as_it_did_before_the_plugin() {
    let text = "Re-read my request. Reply DONE if it is all done.";
    let nudge = ContinueSettings {
        text: text.into(),
        sha256: sha(text.as_bytes()),
        max_nudges: 3,
    };
    let mut d = start("nudge_done", Some(nudge)).await;
    with_members(&mut d, json!({"goal": {"enabled": false}}));
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), text.to_string()],
        "the nudge is the user's own setting and the goal switch does not touch it"
    );
    assert_eq!(phases(&d), ["off"]);
}

const NUDGE_TEXT: &str = "Re-read my request. Reply DONE if it is all done.";

fn nudge_settings() -> ContinueSettings {
    ContinueSettings {
        text: NUDGE_TEXT.into(),
        sha256: sha(NUDGE_TEXT.as_bytes()),
        max_nudges: 3,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn in_goal_mode_the_nudge_of_the_request_is_off_and_the_loop_runs_by_itself() {
    // The first reply of this scenario does not say DONE and the tree changed, so a nudge would go out in a plain run (the twin below proves it).
    let d = start("goal_nudge_asked", Some(nudge_settings())).await;
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string()],
        "the harness sent no nudge: the goal loop sends the follow-ups"
    );
    assert_eq!(rounds_started(&d), 3, "the engine looped by itself");
    let s = d.summary();
    assert_eq!(
        s.nudge,
        DirectNudge {
            requested: true,
            honored: false
        },
        "the summary says the nudge was asked for and not used"
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn nudge off") && l.contains("goal loop")),
        "{:?}",
        d.log.lines()
    );
    println!(
        "GOAL-NUDGE TRIGGER PRESENT: goal on, nudge asked (max 3), first reply without DONE, nudge-sent=no rounds-started=3 summary=requested+not-honored"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_scenario_with_the_goal_off_sends_the_nudge_and_nothing_loops() {
    // The trigger of the case above: the module, the scenario and the nudge settings are the same; only the goal is off.
    let mut d = start("goal_nudge_asked", Some(nudge_settings())).await;
    with_members(&mut d, json!({"goal": {"enabled": false}}));
    let resp = run_goal(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), NUDGE_TEXT.to_string()]
    );
    assert_eq!(rounds_started(&d), 0);
    assert_eq!(
        d.summary().nudge,
        DirectNudge {
            requested: true,
            honored: true
        }
    );
    println!(
        "GOAL-NUDGE NEGATIVE PRESENT: goal off, same nudge settings, nudge-sent=yes rounds-started=0 summary=requested+honored"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_loop_that_cannot_be_armed_leaves_the_nudge_as_it_was() {
    // The plugin folder holds a file the lock does not know: nothing is placed, the module goes off at startup, and the run is the plain run it
    // was before the plugin, with its nudge.
    let plugins = scratch("goal-nudge-lock").join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let source = plugin_folder();
    for entry in walk(&source) {
        let rel = entry.strip_prefix(&source).unwrap();
        let to = plugins.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(&entry, &to).unwrap();
    }
    std::fs::write(
        plugins.join("goal/index.js"),
        b"// changed after the lock was made\n",
    )
    .unwrap();
    let mut m: BTreeMap<String, String> = BTreeMap::new();
    m.insert(
        "AGENTD_GOAL_PLUGIN_DIR".into(),
        plugins.display().to_string(),
    );
    m.insert("AGENTD_GOAL_POLL_MS".into(), "20".into());
    m.insert("AGENTD_GOAL_CLAIM_WAIT_MS".into(), "300".into());
    let goal = GoalHook::for_engine("cline", GoalSettings::from_lookup(&|k| m.get(k).cloned()))
        .expect("the folder has a lock");
    let d = start("goal_nudge_asked", Some(nudge_settings())).await;
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(phases(&d), ["off"], "the module went off at startup");
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string(), NUDGE_TEXT.to_string()]
    );
    assert_eq!(
        d.summary().nudge,
        DirectNudge {
            requested: true,
            honored: true
        }
    );
}

/// Every file below `dir`, in a stable order.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}
