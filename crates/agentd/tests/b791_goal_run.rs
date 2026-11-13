//! The goal loop the harness drives itself, whole: the real `goal` module with the OpenCode goal engine in the real driver against the
//! fake ACP engine, which plays an engine with no goal plugin of its own: it answers each `session/prompt` from the next turn file of a scenario
//! (`fixtures/engines/fake-acp/transcripts/direct/oc_goal_*`, written by the shape OpenCode v2.2.6 puts on the wire). The module sends the
//! verification prompt after the first round and the continue prompt after the rounds that follow, reads the claim line with the run's nonce
//! from the last message of a round, and ends the run on a claim or on one of the stalls of the machine. The pure pieces are in
//! `b791_goal_harness.rs`, the wire in `b791_goal_wire.rs`.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, Hooks, TurnEnd, TurnVote};
use agentd::engine::goal::goal_engine;
use direct_support::{Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::TurnOutcome;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const NONCE: &str = "TESTNONCE0000000";

fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
        ("AGENTD_GOAL_POLL_MS", "20"),
        ("AGENTD_GOAL_EVENTS_EVERY_S", "0"),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

/// The default test profile (`cline`) runs the fake engine under the real profile's environment and hidden paths. The profile `fake-acp` hides
/// `.fake-acp`, which the fixture repository ignores, and git then refuses the progress fingerprint of the tree (a pathspec that names an ignored
/// path), so the stall rules that read the tree would never fire.
async fn start(scenario: &'static str, env: &[(&'static str, &str)]) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 90,
        engine_env: env.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
        ..Opts::default()
    })
    .await
}

async fn run(d: &Direct, s: GoalSettings) -> harness_protocol::runtime::InvocationResponse {
    run_with_modules(d, s, vec![]).await
}

/// As [`run`] with `later` modules registered after the goal module (a later module's prompt replaces the goal module's).
async fn run_with_modules(
    d: &Direct,
    s: GoalSettings,
    later: Vec<Arc<dyn DirectHook>>,
) -> harness_protocol::runtime::InvocationResponse {
    let goal = GoalHook::new(s, goal_engine("opencode").unwrap());
    goal.use_nonce(NONCE);
    let mut modules: Vec<Arc<dyn DirectHook>> = vec![goal];
    modules.extend(later);
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

/// A module that, once, answers the end of the run's second turn with a prompt of its own.
struct Foreign {
    fired: AtomicBool,
}

impl DirectHook for Foreign {
    fn name(&self) -> &'static str {
        "foreign"
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move {
            if end.n == 2 && !self.fired.swap(true, Ordering::SeqCst) {
                TurnVote::Prompt("Please also run the formatter.".to_string())
            } else {
                TurnVote::Done
            }
        })
    }
}

/// The text of each `session/prompt` the engine received, in order.
fn prompts(d: &Direct) -> Vec<String> {
    d.fake_engine_log()
        .lines()
        .filter(|l| l.starts_with("in ") && l.contains("\"session/prompt\""))
        .map(str::to_string)
        .collect()
}

fn events(d: &Direct, phase: &str) -> Vec<serde_json::Value> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["phase"] == phase)
        .collect()
}

fn phases(d: &Direct) -> Vec<String> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v["phase"].as_str().unwrap().to_string())
        .collect()
}

/// The lines of the run's log that the goal module wrote (for the message of a failed assertion).
fn goal_log(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("goal loop"))
        .collect()
}

fn cancels_written(d: &Direct) -> usize {
    d.fake_engine_log()
        .lines()
        .filter(|l| *l == "notification session/cancel")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_claim_in_the_verification_round_ends_the_run_after_two_prompts() {
    let d = start("oc_goal_claim_round2", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(
        d.summary().outcome,
        DirectOutcome::ReportedDone,
        "{}",
        d.engine_trace()
    );
    let p = prompts(&d);
    assert_eq!(
        p.len(),
        2,
        "the user's prompt and one verification prompt: {p:#?}"
    );
    assert!(p[1].contains("Are you sure"), "{}", p[1]);
    assert!(
        p[1].contains(NONCE),
        "the claim line the prompt asks for holds the run's nonce"
    );
    // the work of the first round is in the tree
    assert!(d.worktree().join("src/a.txt").exists());
    let done = events(&d, "done");
    assert_eq!(done.len(), 1, "{:?}", phases(&d));
    assert_eq!(done[0]["summary"], "wrote src/a.txt");
    assert!(events(&d, "brake").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_round_without_a_claim_is_followed_by_a_continue_prompt() {
    let d = start("oc_goal_claim_round3", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let p = prompts(&d);
    assert_eq!(p.len(), 3, "{p:#?}");
    assert!(p[1].contains("Are you sure"), "{}", p[1]);
    assert!(
        p[2].contains("Continue working toward the goal"),
        "{}",
        p[2]
    );
    assert!(!p[2].contains("Are you sure"), "{}", p[2]);
    assert!(d.worktree().join("src/b.txt").exists());
    // the harness's own prompts are told to the run's events as continues, the way the client counts them
    assert_eq!(events(&d, "kick").len(), 2, "{:?}", phases(&d));
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_claim_line_in_the_first_round_counts_for_nothing_and_the_verification_round_runs() {
    let d = start("oc_goal_early_claim_line", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let p = prompts(&d);
    assert_eq!(p.len(), 2, "the early line did not end the run: {p:#?}");
    let done = events(&d, "done");
    assert_eq!(
        done[0]["summary"], "verified",
        "the claim of the second round, not the first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_claim_line_with_another_nonce_does_not_end_the_run() {
    let d = start("oc_goal_wrong_nonce", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(
        prompts(&d).len(),
        3,
        "the wrong nonce was asked about once more"
    );
    let done = events(&d, "done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["summary"], "done");
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_the_same_blocked_line_twice_ends_the_run_with_the_harness_words() {
    let d = start("oc_goal_blocked_twice", &[]).await;
    let resp = run(&d, settings(&[])).await;
    let s = d.summary();
    assert_ne!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    assert_eq!(prompts(&d).len(), 3, "{resp:?}");
    let brakes = events(&d, "brake");
    assert_eq!(brakes.len(), 1, "{:?}", phases(&d));
    assert_eq!(brakes[0]["rule"], "blocked-twice");
    let text = brakes[0]["text"].as_str().unwrap();
    assert!(text.starts_with("The harness ended the run:"), "{text}");
    assert!(text.contains("OpenCode"), "{text}");
    assert!(
        s.detail
            .as_deref()
            .unwrap_or_default()
            .starts_with("The harness ended the run:")
            || s.label.contains("harness ended the run"),
        "{} / {:?}",
        s.label,
        s.detail
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_three_text_only_rounds_in_a_row_end_the_run_with_the_stall_brake() {
    let d = start("oc_goal_text_only", &[]).await;
    run(&d, settings(&[])).await;
    // The machine counts a round as a change when the tree differs from the round before; the first round is compared with the tree as the
    // module armed, and the plugin loop's own test of the same rule allows the same slack ("4..=40" rounds there).
    let n = prompts(&d).len();
    assert!((3..=4).contains(&n), "{n}\n{:#?}", goal_log(&d));
    let brakes = events(&d, "brake");
    assert_eq!(brakes.len(), 1, "{:?}", phases(&d));
    assert_eq!(brakes[0]["rule"], "text-only-stall");
    assert!(events(&d, "done").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_five_rounds_that_change_no_file_end_the_run() {
    let d = start("oc_goal_idle_rounds", &[]).await;
    run(&d, settings(&[])).await;
    let n = prompts(&d).len();
    assert!(
        (5..=6).contains(&n),
        "{n}: the fifth quiet round ends the run, and the first round may count as a change\n{:#?}",
        goal_log(&d)
    );
    let brakes = events(&d, "brake");
    assert_eq!(brakes.len(), 1, "{:?}", phases(&d));
    assert_eq!(brakes[0]["rule"], "no-progress");
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_rounds_that_keep_changing_files_are_never_capped() {
    // six rounds that each write a file and no claim; the seventh claims. No round count ends the loop before it.
    let d = start("oc_goal_many_rounds", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(prompts(&d).len(), 7);
    assert!(events(&d, "brake").is_empty());
    assert_eq!(events(&d, "done").len(), 1);
    assert!(d.worktree().join("src/f6.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_helpers_that_return_in_time_are_not_cancelled() {
    let d = start("oc_goal_helper_returns", &[]).await;
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(cancels_written(&d), 0);
    assert!(events(&d, "helper-stall").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_helper_call_that_never_answers_is_cancelled_and_the_loop_goes_on() {
    // the first prompt is held with the helper call open until the module cancels it; the limit is two seconds for the test
    let d = start(
        "oc_goal_helper_stall",
        &[
            ("FAKE_ACP_SILENCE_S", "300"),
            ("FAKE_ACP_SILENCE_PROMPTS", "1"),
            ("FAKE_ACP_CANCEL_DELAY_MS", "200"),
        ],
    )
    .await;
    let t0 = std::time::Instant::now();
    let resp = run(&d, settings(&[("AGENTD_GOAL_HELPER_SILENCE_S", "2")])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(cancels_written(&d), 1, "one cancel ended the stalled wait");
    let stalls = events(&d, "helper-stall");
    assert_eq!(stalls.len(), 1, "{:?}", phases(&d));
    assert_eq!(stalls[0]["rule"], "silence");
    assert_eq!(stalls[0]["cancelled"], true);
    assert_eq!(stalls[0]["told"], true);
    let notices = prompts(&d)
        .iter()
        .filter(|l| l.contains("The harness cancelled your helper agents."))
        .count();
    assert_eq!(notices, 1, "the engine was told once");
    assert!(
        prompts(&d)
            .iter()
            .any(|l| l.contains("helper agents (subagent)")),
        "the notice names the engine's own helper tool"
    );
    assert!(events(&d, "brake").is_empty());
    assert_eq!(events(&d, "done").len(), 1, "{:?}", phases(&d));
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_the_goal_switch_off_leaves_a_plain_run() {
    let d = Direct::start(Opts {
        scenario: "oc_goal_claim_round2",
        timeout_s: 60,
        goal: Some(serde_json::json!({"enabled": false})),
        ..Opts::default()
    })
    .await;
    run(&d, settings(&[])).await;
    assert_eq!(
        prompts(&d).len(),
        1,
        "no verification prompt: the run is a plain run"
    );
    assert_eq!(phases(&d), vec!["off".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_fresh_session_asks_again_from_the_verification_prompt() {
    // The engine process exits after its third prompt (once, the cursor is kept); the harness starts a fresh session on a new process. The new session
    // knows nothing of the first one's verification, so the module asks the question again there before it believes a claim.
    let d = Direct::start(Opts {
        scenario: "oc_goal_hand_over_1",
        timeout_s: 90,
        engine_env: vec![
            ("FAKE_ACP_KEEP_CURSOR", "1".into()),
            ("FAKE_ACP_EXIT_AFTER_PROMPT", "3".into()),
            ("FAKE_ACP_RESTART_SCENARIO", "oc_goal_hand_over_2".into()),
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
    let resp = run(&d, settings(&[])).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let s = d.summary();
    // TRIGGER PRESENT: the harness started a second session
    assert_eq!(s.fresh_sessions.len(), 1, "{s:?}\n{}", d.engine_trace());
    assert!(
        phases(&d).contains(&"rearm".to_string()),
        "{:?}",
        phases(&d)
    );
    let asked = prompts(&d)
        .iter()
        .filter(|l| l.contains("Are you sure"))
        .count();
    assert_eq!(
        asked, 2,
        "the verification prompt went out once in each session"
    );
    let done = events(&d, "done");
    assert_eq!(done.len(), 1, "{:?}", phases(&d));
    assert_eq!(done[0]["summary"], "finished in the new session");
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_a_claim_in_a_round_another_modules_prompt_started_is_not_believed() {
    // After the verification round the goal module votes a continue prompt, and a module registered after it replaces that vote with its own. The
    // round the other prompt starts ends with a line that carries the nonce (the model has seen it); it was not started by this module, so it is
    // no claim, and the module asks again.
    let d = start("oc_goal_foreign_prompt", &[]).await;
    let foreign = Arc::new(Foreign {
        fired: AtomicBool::new(false),
    });
    let resp = run_with_modules(&d, settings(&[]), vec![foreign]).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let p = prompts(&d);
    assert_eq!(p.len(), 4, "{p:#?}");
    assert!(p[2].contains("Please also run the formatter."), "{}", p[2]);
    assert!(
        p[3].contains("Continue working toward the goal"),
        "{}",
        p[3]
    );
    let done = events(&d, "done");
    assert_eq!(done.len(), 1, "{:?}", phases(&d));
    assert_eq!(
        done[0]["summary"],
        "the claim of the round this module started"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b791_edits_that_undo_each_other_are_no_progress() {
    // Eight rounds that each rewrite one file, A then B then A again: every round changes the tree from the round before, but from the third
    // round on the tree is one the work has been in already, so the fifth such round ends the run.
    let d = start("oc_goal_cycle", &[]).await;
    run(&d, settings(&[])).await;
    let n = prompts(&d).len();
    assert_eq!(n, 7, "{n}\n{:#?}", goal_log(&d));
    let brakes = events(&d, "brake");
    assert_eq!(brakes.len(), 1, "{:?}", phases(&d));
    assert_eq!(brakes[0]["rule"], "no-progress");
}
