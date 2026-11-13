//! The tool-idle watchdog of the driver: a tool call that the engine started and never reports on, with no line from the
//! engine for the limit, is cancelled and the engine is treated as unresponsive. The limit is off for an engine whose profile does not
//! ask for one, so a long silent test run of that engine is left alone.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::BoxFuture;
use agentd::acp::{AcpError, Activity, Watched};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::hooks::{DirectHook, Hooks, TurnEnd, TurnVote};
use agentd::direct::retry::RetryPolicy;
use agentd::engine::watchdog;
use agentd::events::EventRecorder;
use b790_support::{MODEL, Script, Step, TOP, chunk, engine, methods, tool, tool_done};
use harness_protocol::direct::DirectOutcome;
use std::time::Duration;

fn plan(timeout_s: u64, silence_s: u64) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: "Add percent_change".into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(timeout_s),
        silence: Duration::from_secs(silence_s),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
        engine: "opencode".into(),
    }
}

async fn run(
    steps: Vec<Step>,
    tool_idle: Option<Duration>,
    timeout_s: u64,
) -> (agentd::direct::driver::DriverEnd, b790_support::Received) {
    let (client, received, _) = engine(Script {
        prompts: vec![steps],
        ..Script::default()
    });
    let activity = Activity::new();
    activity.set_tool_idle(tool_idle);
    let mut client = client.with_activity(activity.clone());
    let rec = EventRecorder::new("run1", "direct");
    let mut collector = DirectCollector::new(rec);
    let end = run_direct_prompt(
        &mut client,
        &mut collector,
        &activity,
        &plan(timeout_s, 900),
        &NoProbe,
    )
    .await;
    // The engine's reader task records the cancel a moment after it was written.
    tokio::time::sleep(Duration::from_millis(10)).await;
    (end, received)
}

#[tokio::test(start_paused = true)]
async fn b790_a_tool_call_with_no_line_for_the_limit_is_cancelled_and_the_engine_called_unresponsive()
 {
    let (end, received) = run(
        vec![tool("t1", "task", "in_progress"), Step::Silent],
        Some(Duration::from_secs(3600)),
        20_000,
    )
    .await;
    // The silence clock of the provider never ran (a tool call was running); the tool-idle clock ended the stage at its limit.
    assert!(
        matches!(
            end.raw.watched,
            Watched::Done(Err(AcpError::ToolStuck { .. }))
        ),
        "{:?}",
        end.raw.watched
    );
    assert!(
        methods(&received).contains(&"session/cancel".to_string()),
        "{:?}",
        methods(&received)
    );
    let c = classify(&end.raw);
    assert_eq!(c.outcome, DirectOutcome::StoppedEngineDied);
    let detail = c.detail.unwrap();
    assert!(detail.contains("3600"), "{detail}");
    assert!(detail.contains("tool call"), "{detail}");
}

#[tokio::test(start_paused = true)]
async fn b790_with_no_limit_a_silent_tool_call_runs_to_the_deadline_as_it_always_did() {
    let (end, received) = run(
        vec![tool("t1", "run_commands", "in_progress"), Step::Silent],
        None,
        7200,
    )
    .await;
    assert!(
        matches!(end.raw.watched, Watched::TimedOut),
        "{:?}",
        end.raw.watched
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedTimeout);
    assert!(methods(&received).contains(&"session/cancel".to_string()));
}

#[tokio::test(start_paused = true)]
async fn b790_a_line_from_the_engine_restarts_the_tool_idle_clock() {
    // The tool call runs for 100 minutes, but a line comes every 30 minutes: no stretch reaches the one-hour limit.
    let mut steps = vec![tool("t1", "task", "in_progress")];
    for _ in 0..3 {
        steps.push(Step::Sleep(1800));
        steps.push(chunk("still working"));
    }
    steps.push(tool_done("t1"));
    steps.push(chunk("Done."));
    steps.push(Step::Finish("end_turn"));
    let (end, _) = run(steps, Some(Duration::from_secs(3600)), 20_000).await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn b790_a_silence_after_the_tool_finished_is_the_providers_and_keeps_its_own_limit() {
    let (end, _) = run(
        vec![
            tool("t1", "bash", "in_progress"),
            tool_done("t1"),
            Step::Silent,
        ],
        Some(Duration::from_secs(60)),
        20_000,
    )
    .await;
    // Nothing runs, so the tool clock does not count; the provider-silence limit of 900 s ends the stage.
    assert!(
        matches!(end.raw.watched, Watched::Stalled { .. }),
        "{:?}",
        end.raw.watched
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedSilence);
}

#[tokio::test(start_paused = true)]
async fn b790_a_tool_call_that_ends_in_time_does_not_trip_the_watchdog() {
    let (end, _) = run(
        vec![
            tool("t1", "bash", "in_progress"),
            Step::Sleep(3000),
            tool_done("t1"),
            chunk("Done."),
            Step::Finish("end_turn"),
        ],
        Some(Duration::from_secs(3600)),
        20_000,
    )
    .await;
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::ReportedDone);
}

#[test]
fn b790_the_default_is_off_for_cline_and_one_hour_for_opencode() {
    assert_eq!(watchdog::tool_idle("cline", None), None);
    assert_eq!(
        watchdog::tool_idle("opencode", None),
        Some(Duration::from_secs(3600))
    );
    assert_eq!(watchdog::tool_idle("fake-acp", None), None);
}

#[test]
fn b790_the_setting_overrides_the_default_and_zero_switches_it_off() {
    assert_eq!(
        watchdog::tool_idle("opencode", Some(600)),
        Some(Duration::from_secs(600))
    );
    assert_eq!(watchdog::tool_idle("opencode", Some(0)), None);
    // A setting turns the limit on for an engine that has none by default.
    assert_eq!(
        watchdog::tool_idle("cline", Some(900)),
        Some(Duration::from_secs(900))
    );
    assert_eq!(watchdog::tool_idle("cline", Some(0)), None);
    // The longest run is a day; a larger number is that.
    assert_eq!(
        watchdog::tool_idle("opencode", Some(10_000_000)),
        Some(Duration::from_secs(86_400))
    );
}

#[test]
fn b790_the_agentd_setting_is_read_from_the_environment() {
    let get = |key: &str| match key {
        "AGENTD_DIRECT_TOOL_IDLE_S" => Some("1200".to_string()),
        _ => None,
    };
    let s = agentd::config::DirectConfig::from_vars(&get);
    assert_eq!(s.tool_idle_s, Some(1200));
    let none = agentd::config::DirectConfig::from_vars(&|_: &str| None);
    assert_eq!(none.tool_idle_s, None);
    let bad = agentd::config::DirectConfig::from_vars(&|k: &str| {
        (k == "AGENTD_DIRECT_TOOL_IDLE_S").then(|| "soon".to_string())
    });
    assert_eq!(bad.tool_idle_s, None, "a bad value is the default");
}

/// A module that always asks the driver to hold the run open for a second: the engine goes on by itself (a goal loop).
struct Holder;

impl DirectHook for Holder {
    fn name(&self) -> &'static str {
        "holder"
    }

    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async {
            TurnVote::Wait {
                until: Duration::from_secs(1),
            }
        })
    }

    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async { TurnVote::Done })
    }
}

#[tokio::test(start_paused = true)]
async fn b790_a_tool_call_that_goes_silent_while_the_run_is_held_open_is_cut_like_one_in_a_prompt()
{
    let (client, received, _) = engine(Script {
        // The prompt is answered; then the engine goes on by itself, starts a helper agent and says nothing more.
        prompts: vec![vec![
            chunk("first"),
            Step::Finish("end_turn"),
            tool("t1", "task", "in_progress"),
            Step::Silent,
        ]],
        ..Script::default()
    });
    let activity = Activity::new();
    activity.set_tool_idle(Some(Duration::from_secs(3600)));
    let hooks = Hooks::new(vec![
        std::sync::Arc::new(Holder) as std::sync::Arc<dyn DirectHook>
    ]);
    let mut client = client
        .with_activity(activity.clone())
        .with_tap(hooks.tap().unwrap());
    let mut collector =
        DirectCollector::new(EventRecorder::new("run1", "direct")).with_hooks(hooks);
    let end = run_direct_prompt(
        &mut client,
        &mut collector,
        &activity,
        &plan(20_000, 900),
        &NoProbe,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        matches!(
            end.raw.watched,
            Watched::Done(Err(AcpError::ToolStuck { .. }))
        ),
        "{:?}",
        end.raw.watched
    );
    assert!(
        methods(&received).contains(&"session/cancel".to_string()),
        "{:?}",
        methods(&received)
    );
    assert_eq!(classify(&end.raw).outcome, DirectOutcome::StoppedEngineDied);
}
