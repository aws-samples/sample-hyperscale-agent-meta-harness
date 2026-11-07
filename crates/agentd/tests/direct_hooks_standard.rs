//! The modules of the product and the order the driver asks them in: `Hooks::standard` is the one place that
//! says it, and `direct_hooks_unit.rs` checks registration order for fakes only. The order is part of the contract: the votes of a turn
//! end are taken in it (a `Stop` of an earlier module is final, a later `Prompt` replaces an earlier one), so `messages` is last on
//! purpose and a pending message of the user wins over the continuation another module proposes.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{HookEnv, Hooks};
use agentd::events::{EventRecorder, TurnControls};
use common::plugin_folder;
use direct_support::{Direct, Opts, RUN};

/// The ten modules of the product, in the order they are asked.
const ORDER: [&str; 10] = [
    "transcript",
    "goal",
    "research",
    "tools",
    "attachments",
    "engine_config",
    "skills",
    "continue_from",
    "report",
    "messages",
];

/// The goal settings of a host that has the plugin folder: one made like the image's (`common::plugin_folder`, with its `lock.json`).
fn goal_with_plugins() -> GoalSettings {
    let dir = plugin_folder().display().to_string();
    GoalSettings::from_lookup(&move |k| (k == "AGENTD_GOAL_PLUGIN_DIR").then(|| dir.clone()))
}

fn goal_switched_off() -> GoalSettings {
    GoalSettings::from_lookup(&|k| (k == "AGENTD_GOAL").then(|| "off".to_string()))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_product_asks_its_ten_modules_in_this_order() {
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    let hooks = Hooks::standard_with(&env, goal_with_plugins());
    assert_eq!(
        hooks.names(),
        ORDER,
        "the order of the votes is the order of this list"
    );
    assert_eq!(
        hooks.names().last(),
        Some(&"messages"),
        "messages is asked last, so a user's pending message beats another module's continuation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_comes_after_research() {
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    let hooks = Hooks::standard_with(&env, goal_with_plugins());
    let names = hooks.names();
    let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert_eq!(at("tools"), at("research") + 1, "{names:?}");
    // The settings the user sent are placed after it and the server is stopped by `on_shutdown` whatever they do (the order matters for that:
    // a refusal of engine_config comes after the tool hook's startup, so the tool hook must clean up on its own).
    assert!(at("tools") < at("engine_config"), "{names:?}");
    // It sits in front of the modules that vote, so a `Stop` of an earlier module stays final and `messages` stays last.
    assert!(at("tools") < at("messages"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_is_off_leaves_the_others_in_their_order() {
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    let hooks = Hooks::standard_with(&env, goal_switched_off());
    let want: Vec<&str> = ORDER.iter().copied().filter(|n| *n != "goal").collect();
    assert_eq!(hooks.names(), want, "the goal loop is off for this run");
    assert!(
        hooks.names().contains(&"tools"),
        "the tool hook is not the goal loop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_constructor_keeps_the_same_order_whatever_the_host_has() {
    // `standard` reads the goal plugin folder from the host (`AGENTD_GOAL_PLUGIN_DIR`, default /opt/harness/engine/plugins), which a build
    // host may not have: then the goal module is absent and the other nine are in their order.
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    let names = Hooks::standard(&env).names();
    let want: Vec<&str> = ORDER
        .iter()
        .copied()
        .filter(|n| *n != "goal" || names.contains(n))
        .collect();
    assert_eq!(names, want);
    assert!(names.len() >= 9, "{names:?}");
}
