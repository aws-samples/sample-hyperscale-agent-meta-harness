//! The goal loop, whole: the real `goal` module in the real driver against the fake ACP engine, which
//! emulates the engine's goal plugin and the harness's observer (`goal.json` in a scenario directory: after the first prompt the engine goes
//! on by itself with follow-up rounds, writes the rounds log and the state file, ignores `session/cancel`, queues a prompt that arrives
//! during a round, and counts a call of the completion tool only after a verification round, as the plugin does). The scenarios are the
//! offline versions of the fit study's 27: loop until the goal is complete, BLOCKED twice, a
//! text-only stall, rounds that change nothing, a loop that dies, a runaway, a user's message during a round, the user's cancel, the silence
//! limit, a goal the engine never claims, a missing observer, a call that came too early, and a plugin folder that does not match its lock.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::files::place_plugins;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::state::{GoalState, pending_state, read_state, write_state};
use agentd::direct::hooks::{DirectHook, Event, Hooks, StartupCtx, TurnEnd, TurnVote, Wire};
use common::plugin_folder;
use direct_support::{Direct, Opts, PROMPT};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The settings of a test: a plugin folder like the image's (`common::plugin_folder`) and times short enough to run in seconds.
fn settings_at(plugin_dir: &Path, extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
        ("AGENTD_GOAL_PLUGIN_DIR", plugin_dir.display().to_string()),
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
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    settings_at(&plugin_folder(), extra)
}

async fn run_with_modules(d: &Direct, modules: Vec<Arc<dyn DirectHook>>) -> InvocationResponse {
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

async fn run_goal(
    d: &Direct,
    s: GoalSettings,
    others: Vec<Arc<dyn DirectHook>>,
) -> InvocationResponse {
    let goal = GoalHook::for_engine("cline", s).expect("the plugin folder is in the repository");
    let mut modules: Vec<Arc<dyn DirectHook>> = vec![goal];
    modules.extend(others);
    run_with_modules(d, modules).await
}

async fn start(scenario: &'static str) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 60,
        ..Opts::default()
    })
    .await
}

/// The lines of the observer's log, as the engine wrote them.
fn rounds(d: &Direct) -> Vec<serde_json::Value> {
    std::fs::read_to_string(
        d.agent_data()
            .join("data/plugins/harness-rounds/rounds.jsonl"),
    )
    .unwrap_or_default()
    .lines()
    .map(|l| serde_json::from_str(l).unwrap())
    .collect()
}

fn count_ev(d: &Direct, ev: &str) -> usize {
    rounds(d).iter().filter(|r| r["ev"] == ev).count()
}

fn state(d: &Direct) -> GoalState {
    read_state(&d.agent_data().join("data/plugins/goal/goals.json"))
}

/// The phases of the `goal-loop` progress events, in order.
fn phases(d: &Direct) -> Vec<String> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v["phase"].as_str().unwrap().to_string())
        .collect()
}

fn cancels_written(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

fn detail(d: &Direct) -> String {
    d.summary().detail.clone().unwrap_or_default()
}

/// The number in "... N rounds in a row ...".
fn rounds_in_a_row(detail: &str) -> u32 {
    let before = detail.split(" rounds in a row").next().unwrap();
    before.rsplit(' ').next().unwrap().parse().unwrap()
}

const FIRST_MESSAGE: &str = "Added percent_change with tests.";

// ---- the loop runs until the goal is complete --------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_loop_runs_until_the_engine_completes_the_goal_and_the_run_ends_with_the_engines_own_words()
 {
    let d = start("goal_loop_passes").await;
    let resp = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // the engine's last message is the run's message, not the first prompt's
    assert_eq!(
        s.cline_message.as_deref(),
        Some("Goal marked complete."),
        "{}",
        d.fake_engine_log()
    );
    // ONE prompt went to the engine: the rest of the work was the engine's own loop
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    // TRIGGER PRESENT: the engine looped by itself (three rounds), which a run without the module does not (see the goal-off case)
    assert_eq!(
        (count_ev(&d, "start"), count_ev(&d, "end")),
        (3, 3),
        "{:?}",
        rounds(&d)
    );
    // the plugin removed the goal when it was completed
    assert_eq!(
        state(&d),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    assert_eq!(
        cancels_written(&d),
        0,
        "the loop's rounds ignore session/cancel: it is not used to end a loop"
    );
    let p = phases(&d);
    assert_eq!(p.first().map(String::as_str), Some("on"), "{p:?}");
    assert_eq!(p.last().map(String::as_str), Some("done"), "{p:?}");
    // the events carry their numbers and words as fields, so a screen need not read the sentences
    let events: Vec<_> = d
        .aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    let rounds_seen: Vec<_> = events.iter().filter(|v| v["phase"] == "round").collect();
    assert!(!rounds_seen.is_empty(), "{events:?}");
    assert!(
        rounds_seen
            .iter()
            .all(|v| v["rounds"].as_u64().is_some_and(|n| n >= 1) && v["status"].is_string()),
        "{rounds_seen:?}"
    );
    let done = events.iter().find(|v| v["phase"] == "done").unwrap();
    assert_eq!(done["summary"], "all done", "{done}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_and_the_observer_are_placed_read_only_with_the_plugin_mode_set_and_the_goal_written_before_the_first_prompt()
 {
    let d = start("goal_loop_passes").await;
    let plugins = plugin_folder();
    let _ = run_goal(&d, settings_at(&plugins, &[]), vec![]).await;
    let config = d.agent_data().join("config/plugins");
    for rel in [
        "goal/index.js",
        "goal/package.json",
        "goal/LICENSE",
        "goal/NOTICE",
        "goal/PROVENANCE",
        "harness-rounds/index.js",
        "harness-rounds/package.json",
    ] {
        let placed = config.join(rel);
        assert_eq!(
            std::fs::read(&placed).unwrap(),
            std::fs::read(plugins.join(rel)).unwrap(),
            "{rel}"
        );
        assert_eq!(
            std::fs::metadata(&placed).unwrap().permissions().mode() & 0o777,
            0o444,
            "{rel}"
        );
    }
    for dir in ["goal", "harness-rounds", "."] {
        assert_eq!(
            std::fs::metadata(config.join(dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555,
            "{dir}"
        );
    }
    assert!(
        d.engine_probe().contains("CLINE_PLUGIN_MODE=in_process"),
        "{}",
        d.engine_probe()
    );
    // the goal text the engine was given (the log of the fake engine keeps what it read from the state file at its first run)
    assert!(
        d.fake_engine_log().contains("goal claimed"),
        "{}",
        d.fake_engine_log()
    );
    // the user's first prompt went out as typed
    assert_eq!(d.prompts_received()[0], PROMPT);
    for dir in ["goal", "harness-rounds", "."] {
        std::fs::set_permissions(config.join(dir), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_check_is_fixed_and_checked_again_until_the_goal_is_complete() {
    let d = start("goal_loop_fixes_then_passes").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert_eq!(count_ev(&d, "end"), 4, "{:?}", rounds(&d));
    // the fix the engine made in round 2 is in the work tree the user receives
    let calc = std::fs::read_to_string(d.worktree().join("src/calc.py")).unwrap();
    assert!(calc.contains("fixed in round 2"), "{calc}");
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_of_the_completion_tool_that_the_plugin_refused_is_not_a_done_claim() {
    let d = start("goal_early_claim_refused").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // round 2 called the tool and was refused (`completed:false`); the run went on to round 4 where the plugin accepted it
    assert_eq!(count_ev(&d, "end"), 4, "{:?}", rounds(&d));
    assert!(
        d.fake_engine_log().contains("completion refused"),
        "{}",
        d.fake_engine_log()
    );
}

// ---- the brakes --------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_identical_blocked_replies_end_the_run_and_the_engines_loop_is_stopped_by_clearing_its_goal()
 {
    let d = start("goal_blocked_twice").await;
    let resp = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    let detail = detail(&d);
    assert!(
        detail.starts_with("The harness ended the run: Cline ended "),
        "{detail}"
    );
    assert!(
        detail.ends_with(
            " rounds in a row with the same last line. Its words: \"BLOCKED: need the API key\""
        ),
        "{detail}"
    );
    // rounds that end within one read of the log are counted together, so the count can pass the limit of 2
    assert!(rounds_in_a_row(&detail) >= 2, "{detail}");
    // the loop is stopped: a few rounds ran, not an endless number (the scenario repeats its last round for ever)
    let ends = count_ev(&d, "end");
    assert!((3..=40).contains(&ends), "{ends}");
    assert_eq!(
        state(&d),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    assert_eq!(cancels_written(&d), 0);
    let p = phases(&d);
    assert!(p.contains(&"brake".to_string()), "{p:?}");
    let brake = d
        .aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["phase"] == "brake")
        .unwrap();
    assert_eq!(brake["rule"], "blocked-twice", "{brake}");
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    // no label says the work is good
    for banned in ["verified", "tested", "gated", "evaluated"] {
        assert!(!detail.to_lowercase().contains(banned), "{detail}");
    }
    // the label of the card says the harness ended the run, from the outcome and the detail alone
    assert_eq!(s.label, s.outcome.label_for(s.detail.as_deref()));
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run (Cline ended "),
        "{}",
        s.label
    );
    assert!(
        !s.label.contains("unknown stop reason") && !s.label.contains("BLOCKED"),
        "{}",
        s.label
    );
    // the final event of agentd carries the same label
    let ended = d.events_of(harness_protocol::envelope::EventType::TurnEnded);
    assert_eq!(
        ended.last().unwrap()["label"],
        s.label.as_str(),
        "{ended:?}"
    );
}

/// What the goal module does for the first prompt, with none of the brakes: it places the plugins and the goal and then holds the run open
/// for ever. The baseline that shows the scenarios would not end on their own.
struct NoBrake;

impl DirectHook for NoBrake {
    fn name(&self) -> &'static str {
        "no-brake"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            place_plugins(
                &plugin_folder(),
                ctx.config_dir,
                &["goal", "harness-rounds"],
            )?;
            let state = ctx.data_dir.join("plugins/goal/goals.json");
            std::fs::create_dir_all(state.parent().unwrap()).map_err(|e| e.to_string())?;
            write_state(&state, &pending_state("goal", "2026-10-16T00:00:00Z"), None)
                .map_err(|e| e.to_string())?;
            ctx.engine_env
                .insert("CLINE_PLUGIN_MODE".into(), "in_process".into());
            Ok(())
        })
    }

    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async {
            TurnVote::Wait {
                until: std::time::Duration::from_millis(50),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_present_without_the_brakes_the_blocked_scenario_never_ends_on_its_own() {
    let d = Direct::start(Opts {
        scenario: "goal_blocked_twice",
        timeout_s: 5,
        ..Opts::default()
    })
    .await;
    let _ = run_with_modules(&d, vec![Arc::new(NoBrake)]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::StoppedTimeout,
        "{}",
        d.engine_trace()
    );
    assert!(count_ev(&d, "end") > 15, "{}", count_ev(&d, "end"));
}

#[tokio::test(flavor = "multi_thread")]
async fn three_text_only_rounds_with_an_unchanged_tree_end_the_run() {
    let d = start("goal_text_only_stall").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    let detail = detail(&d);
    assert!(
        detail.starts_with("The harness ended the run: "),
        "{detail}"
    );
    assert!(
        detail.ends_with(" rounds in a row had no tool call and changed no file."),
        "{detail}"
    );
    // rounds that end within one read of the log are counted together, so the count can pass the limit of 3
    assert!(rounds_in_a_row(&detail) >= 3, "{detail}");
    let ends = count_ev(&d, "end");
    assert!((4..=40).contains(&ends), "{ends}");
}

#[tokio::test(flavor = "multi_thread")]
async fn five_rounds_with_tool_calls_and_no_change_end_the_run() {
    let d = start("goal_idle_rounds").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(
        detail(&d).ends_with(" rounds in a row changed no file."),
        "{}",
        detail(&d)
    );
    assert!(rounds_in_a_row(&detail(&d)) >= 5, "{}", detail(&d));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loop_that_died_gets_one_continue_through_the_drivers_sender_and_runs_on() {
    let d = start("goal_dead_loop_then_done").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // the second prompt of the run is the continue, sent as an instruction in the same session
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert_eq!(prompts[0], PROMPT);
    assert_eq!(prompts[1], agentd::direct::goal::KICK_PROMPT);
    assert!(phases(&d).contains(&"kick".to_string()), "{:?}", phases(&d));
    assert_eq!(d.engine_sessions().len(), 0, "no fresh session was needed");
}

/// The user can leave the sentence about work outside the checkout out of the goal prompts (`goal.outsideCheckoutNote: false` in the
/// request). The goal text the loop is armed with and the continue prompt both lose it; the rest of the prompts stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_turns_the_outside_sentence_off_gets_the_plain_continue_and_the_shorter_goal_text()
 {
    let d = Direct::start(Opts {
        scenario: "goal_dead_loop_then_done",
        timeout_s: 60,
        goal: Some(serde_json::json!({"enabled": true, "outsideCheckoutNote": false})),
        ..Opts::default()
    })
    .await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    let plain = agentd::direct::goal::kick_prompt(false);
    assert_eq!(prompts[1], plain);
    assert!(!prompts[1].to_lowercase().contains("outside this checkout"));
    assert!(phases(&d).contains(&"kick".to_string()), "{:?}", phases(&d));
    // TRIGGER PRESENT: the loop was armed with the goal text the request left the sentence out of.
    let bytes = agentd::direct::goal::settings::strip_outside_note(
        agentd::direct::goal::settings::DEFAULT_GOAL_TEXT,
    )
    .len();
    let armed = format!("goal text {bytes} bytes");
    assert!(
        d.log.lines().iter().any(|l| l.contains(&armed)),
        "{armed}: {:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runaway_whose_rounds_the_engine_stops_again_and_again_gets_two_continues_and_then_the_run_ends()
 {
    let d = start("goal_runaway_aborts").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    let prompts = d.prompts_received();
    assert_eq!(
        prompts
            .iter()
            .filter(|p| *p == agentd::direct::goal::KICK_PROMPT)
            .count(),
        2,
        "{prompts:?}"
    );
    let detail = detail(&d);
    assert!(
        detail.contains("stopped 2 times in a row with no change to the files"),
        "{detail}"
    );
    assert!(detail.contains("aborted"), "{detail}");
}

// ---- no loop ------------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_never_claims_the_goal_runs_as_a_plain_run_and_the_module_says_so() {
    let d = start("goal_not_claimed").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert_eq!(s.cline_message.as_deref(), Some(FIRST_MESSAGE));
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    assert_eq!(phases(&d), ["off"]);
    let (_, off) = d.aws.progress("goal-loop").remove(0);
    assert!(
        off["text"]
            .as_str()
            .unwrap()
            .contains("did not claim the goal"),
        "{off}"
    );
    // nothing is left in the state file for a later session to claim
    assert_eq!(
        state(&d),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn goal loop off")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_goal_loop_off_a_run_is_what_it_was_no_plugin_no_state_file_one_prompt() {
    assert!(GoalHook::for_engine("cline", settings(&[("AGENTD_GOAL", "off")])).is_none());
    assert!(
        GoalHook::for_engine("fake-acp", settings(&[])).is_none(),
        "an engine with no goal plugin"
    );
    assert!(
        GoalHook::for_engine("cline", settings_at(Path::new("/nonexistent"), &[])).is_none(),
        "an image without the plugin folder"
    );
    let d = start("goal_loop_passes").await;
    let resp = run_with_modules(&d, vec![]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.cline_message.as_deref(), Some(FIRST_MESSAGE));
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    assert!(!d.agent_data().join("config/plugins").exists());
    assert!(!d.agent_data().join("data/plugins").exists());
    assert!(!d.engine_probe().contains("CLINE_PLUGIN_MODE"));
    assert!(d.aws.progress("goal-loop").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_folder_that_does_not_match_its_lock_is_not_placed_and_the_run_goes_on_without_the_loop()
 {
    let d = start("goal_loop_passes").await;
    let changed_plugins = d.dir.join("plugins-changed");
    copy_dir(&plugin_folder(), &changed_plugins);
    std::fs::write(
        changed_plugins.join("goal/index.js"),
        "export default { name: 'goal', hooks: {} };\n",
    )
    .unwrap();
    let resp = run_goal(&d, settings_at(&changed_plugins, &[]), vec![]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    assert!(
        !d.agent_data().join("config/plugins").exists(),
        "nothing is placed when one file does not match"
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn goal loop off")
                && l.contains("goal/index.js does not match lock.json")),
        "{:?}",
        d.log.lines()
    );
    assert!(
        !d.agent_data().join("data/plugins/goal/goals.json").exists()
            || state(&d)
                != GoalState::Seen {
                    pending: true,
                    active: 0
                }
    );
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "build" {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::create_dir_all(to).unwrap();
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_observers_log_the_done_claim_still_ends_the_run_after_a_quiet_engine_and_the_module_says_it_works_without_the_log()
 {
    let d = start("goal_no_observer").await;
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    assert!(rounds(&d).is_empty(), "the scenario writes no rounds log");
    assert!(
        phases(&d).contains(&"fallback".to_string()),
        "{:?}",
        phases(&d)
    );
}

// ---- a user's message, the user's cancel, the silence limit -------------------------------------------------------------------------------

/// A second module (the messages module's place in the order): votes one prompt at its first turn end.
struct Messenger {
    sent: AtomicBool,
    asks: AtomicU32,
}

impl DirectHook for Messenger {
    fn name(&self) -> &'static str {
        "messenger"
    }
    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            self.asks.fetch_add(1, Ordering::SeqCst);
            if self.sent.swap(true, Ordering::SeqCst) {
                TurnVote::Done
            } else {
                TurnVote::Prompt("Please also add a README line.".into())
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_users_message_during_the_loop_goes_out_once_through_the_one_sender_and_the_loop_goes_on_to_the_end()
 {
    let d = start("goal_user_message").await;
    let m = Arc::new(Messenger {
        sent: AtomicBool::new(false),
        asks: AtomicU32::new(0),
    });
    let _ = run_goal(&d, settings(&[]), vec![m.clone()]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // the goal's own prompts and the user's message share the driver's sender: one prompt at a time, in order
    assert_eq!(
        d.prompts_received(),
        [
            PROMPT.to_string(),
            "Please also add a README line.".to_string()
        ]
    );
    // the engine queued the message behind the round in flight (its answer came at once) and the loop still ended with the goal complete
    assert!(
        d.fake_engine_log().contains("prompt queued during a round"),
        "{}",
        d.fake_engine_log()
    );
    assert_eq!(
        d.summary().cline_message.as_deref(),
        Some("Goal marked complete.")
    );
    assert!(
        m.asks.load(Ordering::SeqCst) >= 2,
        "the messages module is asked again while the goal module holds the run"
    );
}

/// Raises the user's cancel when the engine says a marker.
struct CancelOn {
    marker: &'static str,
    controls: Mutex<Option<Arc<agentd::events::TurnControls>>>,
}

impl DirectHook for CancelOn {
    fn name(&self) -> &'static str {
        "cancel-on"
    }
    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.controls.lock().unwrap() = Some(ctx.env.controls.clone());
            Ok(())
        })
    }
    fn on_wire(&self, w: &Wire<'_>) {
        if w.msg.to_string().contains(self.marker)
            && let Some(c) = self.controls.lock().unwrap().clone()
        {
            c.deliver(&[PendingControl::Cancel]);
        }
    }
    fn on_event(&self, _e: &Event<'_>) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_cancel_during_a_round_stops_the_loop_by_clearing_the_goal_and_the_stop_is_the_users()
 {
    let d = start("goal_cancel_midloop").await;
    let c = Arc::new(CancelOn {
        marker: "ROUND-TWO-WORKING",
        controls: Mutex::new(None),
    });
    let resp = run_goal(&d, settings(&[]), vec![c]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Cancelled,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedByUser);
    assert_eq!(cancels_written(&d), 1);
    assert_eq!(
        state(&d),
        GoalState::Seen {
            pending: false,
            active: 0
        },
        "the loop is told to stop: the round's own cancel is ignored by the engine"
    );
    // no round started after the cancel (the scenario would run on for ever)
    let starts = count_ev(&d, "start");
    assert!(starts <= 3, "{starts}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_that_goes_silent_ends_the_run_at_the_silence_limit_like_a_prompt_does() {
    let d = Direct::start(Opts {
        scenario: "goal_silent_round",
        timeout_s: 60,
        provider_silence_s: Some(2),
        ..Opts::default()
    })
    .await;
    let t0 = std::time::Instant::now();
    let _ = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::StoppedSilence,
        "{}",
        d.engine_trace()
    );
    assert!(t0.elapsed().as_secs() < 40, "{:?}", t0.elapsed());
    // the silence ladder ran as for a prompt: a cancel at the stall (the loop's rounds ignore it), one resume sentence, a second silence
    // and the end; the stall is not the user's stop, so the loop is not told to stop (the engine is reaped with the run)
    assert_eq!(cancels_written(&d), 2, "{}", d.engine_trace());
    assert_eq!(d.prompts_received().len(), 2, "{:?}", d.prompts_received());
    assert!(
        d.aws
            .progress("goal-loop")
            .iter()
            .all(|(_, v)| v["phase"] != "brake"),
        "{:?}",
        phases(&d)
    );
}

#[test]
fn the_module_exists_only_for_an_engine_with_a_plugin_a_plugin_folder_and_the_switch_on() {
    assert!(GoalHook::for_engine("fake-acp", settings(&[])).is_none());
    assert!(GoalHook::for_engine("cline", settings(&[("AGENTD_GOAL", "off")])).is_none());
    let nowhere = settings_at(Path::new("/nonexistent/plugins"), &[]);
    assert!(GoalHook::for_engine("cline", nowhere).is_none());
    let hook = GoalHook::for_engine("cline", settings(&[])).unwrap();
    assert_eq!(hook.name(), "goal");
    assert!(format!("{hook:?}").contains("GoalHook"));
}

/// A fresh session (the saved conversation was rejected, so the harness starts session 2 by itself) has no goal: the engine claims goals
/// per session, so the module writes the goal again, and the new session claims it and finishes the loop.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_session_gets_the_goal_written_again_and_the_new_session_claims_it() {
    let d = Direct::start(Opts {
        scenario: "goal_fresh_session",
        timeout_s: 60,
        engine_env: vec![
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done".into()),
            ("FAKE_ACP_RESTART_SCENARIO", "fresh_session_done".into()),
        ],
        edit_cfg: Some(Box::new(|c| {
            c.direct.provider_retries = 3;
            c.direct.engine_stop_resumes = 1;
            c.direct.provider_retry_pause = std::time::Duration::from_millis(10);
            c.direct.input_margin = std::time::Duration::ZERO;
            c.direct.input_min_window = std::time::Duration::from_secs(1);
        })),
        ..Opts::default()
    })
    .await;
    let resp = run_goal(&d, settings(&[]), vec![]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    // TRIGGER PRESENT: the harness started a second session
    assert_eq!(s.fresh_sessions.len(), 1, "{s:?}\n{}", d.engine_trace());
    let p = phases(&d);
    assert!(p.contains(&"rearm".to_string()), "{p:?}");
    // the new session claimed the goal that was written again for it
    let log = d.fake_engine_log();
    assert!(
        log.contains("goal claimed for fake-session-0002"),
        "{log}\n{p:?}"
    );
    assert!(p.contains(&"done".to_string()), "{p:?}");
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
}

// ---- the brake inside one round --------------------------------------------------------------------

/// The label of a stop by the no-progress brake inside a round.
const POLLING_LABEL: &str = "Cline stopped: the harness ended the run (Cline made no progress in ";

fn brake_rule(d: &Direct) -> serde_json::Value {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .find(|v| v["phase"] == "brake")
        .expect("a brake event")["rule"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn varied_polling_calls_inside_one_follow_up_round_end_the_run_with_a_label_that_says_no_progress()
 {
    let d = start("goal_round_polling").await;
    let t0 = std::time::Instant::now();
    let resp = run_goal(&d, settings(&[("AGENTD_GOAL_LOOK_EVERY_MS", "50")]), vec![]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(&d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(POLLING_LABEL), "{}", s.label);
    assert!(
        s.label
            .ends_with(" tool calls in a row: no file changed and no new text appeared)"),
        "{}",
        s.label
    );
    // TRIGGER PRESENT: the engine made the calls inside ONE follow-up round (the first run ended, the second never did) and they were varied
    assert_eq!(
        (count_ev(&d, "start"), count_ev(&d, "end")),
        (2, 1),
        "{:?}",
        rounds(&d)
    );
    assert!(s.tool_calls >= 40, "{}", s.tool_calls);
    let calls: std::collections::BTreeSet<_> = d
        .fake_engine_log()
        .lines()
        .filter(|l| l.starts_with("out ") && l.contains("\"sessionUpdate\":\"tool_call\""))
        .map(str::to_string)
        .collect();
    assert!(calls.len() >= 40, "{}", calls.len());
    assert_eq!(brake_rule(&d), "round-no-progress");
    assert_eq!(
        state(&d),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    assert_eq!(
        d.prompts_received(),
        [PROMPT.to_string()],
        "no continue was sent"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(25),
        "{:?}",
        t0.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn varied_polling_calls_inside_the_first_prompt_are_cancelled_and_end_the_run() {
    let d = start("goal_first_polling").await;
    let t0 = std::time::Instant::now();
    let resp = run_goal(&d, settings(&[("AGENTD_GOAL_LOOK_EVERY_MS", "50")]), vec![]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}\n{:#?}",
        d.engine_trace(),
        phases(&d),
        d.log.lines()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}",
        d.engine_trace()
    );
    assert!(s.label.starts_with(POLLING_LABEL), "{}", s.label);
    // TRIGGER PRESENT: the 40 calls were the first prompt's own run, which would have held the run to its deadline of 60 s
    assert!(s.tool_calls >= 40, "{}", s.tool_calls);
    assert_eq!(cancels_written(&d), 1, "the open prompt was cancelled once");
    assert_eq!(brake_rule(&d), "round-no-progress");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(25),
        "{:?}",
        t0.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_that_come_with_a_changed_file_are_not_polling() {
    let d = start("goal_round_edits").await;
    let resp = run_goal(&d, settings(&[("AGENTD_GOAL_LOOK_EVERY_MS", "50")]), vec![]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    // TRIGGER PRESENT: the same 40 varied calls as the polling scenario, and a file changed in the round
    assert!(s.tool_calls >= 40, "{}", s.tool_calls);
    assert!(d.worktree().join("notes/a.txt").exists());
    assert!(
        !phases(&d).contains(&"brake".to_string()),
        "{:?}",
        phases(&d)
    );
}
