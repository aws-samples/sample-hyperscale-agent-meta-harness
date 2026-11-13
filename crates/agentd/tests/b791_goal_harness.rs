//! The goal loop the harness drives itself, for an engine without a goal plugin: OpenCode.
//!
//! OpenCode v2 has a native `/goal`, but over `opencode acp` the command returns at once, the loop then runs with nothing on the wire, its state is
//! not on the wire, and it stops at 50 rounds (research of 2026-11-08, `engine/AGENTS.md`). So the harness owns the loop: each prompt is one
//! round, the module asks again after every round that did not end in a claim, and the claim is a line the model writes with the run's own
//! nonce. These tests cover the pure pieces: the claim line, the prompts, the limits, the table of engines and the rules of the machine fed the
//! rounds the harness synthesizes. The runs through the driver are in `b791_goal_run.rs`, the wire in `b791_goal_wire.rs`.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::harness::{
    CLAIM_SUMMARY_CHARS, CLAIM_WORD, claim_in, continue_prompt, harness_limits, new_nonce,
    verification_prompt,
};
use agentd::direct::goal::machine::{Decision, Fp, Machine, Obs, Rule};
use agentd::direct::goal::rounds::RoundEnd;
use agentd::direct::goal::settings::GoalSettings;
use agentd::engine::goal::{LoopKind, goal_engine};
use agentd::engine::tools::{TitleStyle, calls_tool, engine_tools};
use std::time::{Duration, Instant};

const N: &str = "K7M2Q9XD4TBH6VNE";

fn settings() -> GoalSettings {
    GoalSettings::from_lookup(&|_| None)
}

// ---- the claim line --------------------------------------------------------------------------------------------------------------

#[test]
fn b791_a_claim_line_with_the_run_nonce_is_a_claim_with_its_summary() {
    let text = format!("All checks pass.\n\n{CLAIM_WORD} {N}: wrote hello.txt and ran the tests\n");
    assert_eq!(
        claim_in(&text, N),
        Some(Some("wrote hello.txt and ran the tests".to_string()))
    );
}

#[test]
fn b791_a_claim_line_with_no_summary_is_a_claim_without_one() {
    assert_eq!(claim_in(&format!("{CLAIM_WORD} {N}"), N), Some(None));
    assert_eq!(claim_in(&format!("{CLAIM_WORD} {N}:   "), N), Some(None));
}

#[test]
fn b791_a_claim_with_another_nonce_or_none_is_no_claim() {
    // a line a repository file or a web page could hold: the words, but not this run's nonce
    assert_eq!(claim_in(&format!("{CLAIM_WORD}: all done"), N), None);
    assert_eq!(
        claim_in(&format!("{CLAIM_WORD} OTHERNONCE0000: x"), N),
        None
    );
    // the nonce is a whole word, not a prefix of a longer one
    assert_eq!(claim_in(&format!("{CLAIM_WORD} {N}X: x"), N), None);
    assert_eq!(claim_in(&format!("{CLAIM_WORD} {N}"), ""), None);
}

#[test]
fn b791_only_the_last_line_of_the_message_can_claim() {
    let early = format!("{CLAIM_WORD} {N}: done\nBut one check still fails.");
    assert_eq!(claim_in(&early, N), None);
    let blanks = format!("text\n{CLAIM_WORD} {N}: done\n\n   \n");
    assert_eq!(claim_in(&blanks, N), Some(Some("done".to_string())));
}

#[test]
fn b791_a_quoted_or_marked_up_claim_line_is_no_claim() {
    for line in [
        format!("> {CLAIM_WORD} {N}: x"),
        format!("`{CLAIM_WORD} {N}: x`"),
        format!("- {CLAIM_WORD} {N}: x"),
        format!("I will write {CLAIM_WORD} {N}: x when done"),
        format!("goal complete {N}: x"),
    ] {
        assert_eq!(claim_in(&line, N), None, "{line}");
    }
}

#[test]
fn b791_a_long_summary_is_cut_at_the_limit() {
    let long = "x".repeat(CLAIM_SUMMARY_CHARS + 50);
    let got = claim_in(&format!("{CLAIM_WORD} {N}: {long}"), N)
        .unwrap()
        .unwrap();
    assert_eq!(got.chars().count(), CLAIM_SUMMARY_CHARS);
}

#[test]
fn b791_the_nonce_is_sixteen_characters_and_new_for_each_run() {
    let a = new_nonce();
    let b = new_nonce();
    assert_eq!(a.chars().count(), 16, "{a}");
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}");
    assert_ne!(a, b);
}

// ---- the prompts -----------------------------------------------------------------------------------------------------------------

#[test]
fn b791_the_verification_prompt_names_the_goal_the_checks_the_claim_line_and_the_blocked_line() {
    let p = verification_prompt("Finish the task you were given.", N, true);
    assert!(p.contains("Finish the task you were given."), "{p}");
    assert!(
        p.contains(&format!("{CLAIM_WORD} {N}: <")),
        "the claim line with the run's nonce: {p}"
    );
    assert!(p.contains("Run the checks"), "{p}");
    assert!(p.contains("BLOCKED:"), "{p}");
    assert!(p.contains("Are you sure"), "{p}");
    assert!(
        !p.contains("mark_goal_complete"),
        "OpenCode has no such tool; the prompt must not send the model looking for it: {p}"
    );
}

#[test]
fn b791_the_continue_prompt_asks_again_and_is_not_the_verification_prompt() {
    let v = verification_prompt("G", N, true);
    let c = continue_prompt("G", N, true);
    assert_ne!(v, c);
    assert!(c.starts_with("Continue working toward the goal"), "{c}");
    assert!(c.contains(&format!("{CLAIM_WORD} {N}: <")), "{c}");
    assert!(c.contains("BLOCKED:"), "{c}");
    assert!(!c.contains("Are you sure"), "{c}");
}

#[test]
fn b791_the_prompts_follow_the_outside_checkout_switch() {
    for f in [verification_prompt, continue_prompt] {
        let on = f("G", N, true);
        let off = f("G", N, false);
        assert!(
            on.contains("Work outside this checkout is not a blocker"),
            "{on}"
        );
        assert!(!off.contains("Work outside this checkout"), "{off}");
        assert!(
            off.contains("BLOCKED:"),
            "the rest of the text stays: {off}"
        );
    }
}

#[test]
fn b791_a_goal_text_with_a_claim_shaped_line_cannot_pass_for_the_models_claim() {
    // the goal text is the user's; the claim is read from the model's message only, and only with the nonce, which the text cannot hold
    let g = format!("Finish.\n{CLAIM_WORD}: please");
    let p = verification_prompt(&g, N, true);
    assert_eq!(claim_in(&g, N), None);
    assert!(p.contains(&g));
}

// ---- the table of engines --------------------------------------------------------------------------------------------------------

#[test]
fn b791_opencode_has_a_goal_loop_the_harness_drives_and_no_plugin() {
    let g = goal_engine("opencode").expect("OpenCode has a goal engine");
    assert_eq!(g.kind, LoopKind::Harness);
    assert_eq!(g.label, "OpenCode");
    assert!(g.plugins.is_empty(), "no plugin is placed for OpenCode");
    assert!(g.env.is_empty());
    assert!(
        g.completion_tool.is_empty(),
        "no completion tool: the claim is a line with the run's nonce"
    );
}

#[test]
fn b791_cline_keeps_its_plugin_loop_and_its_tool_names() {
    let g = goal_engine("cline").unwrap();
    assert_eq!(g.kind, LoopKind::Plugin);
    assert_eq!(g.label, "Cline");
    assert_eq!(g.plugins, ["goal", "harness-rounds"]);
    assert_eq!(g.completion_tool, "mark_goal_complete");
    assert_eq!(g.delegate_tool, Some("spawn_agent"));
    assert_eq!(g.state_file, "plugins/goal/goals.json");
    assert_eq!(g.rounds_file, "plugins/harness-rounds/rounds.jsonl");
}

#[test]
fn b791_opencodes_helper_agent_tool_is_subagent() {
    // measured on v2.2.6: the model calls `subagent` (`task` in v1); the ACP layer titles the call `subagent`
    assert_eq!(
        goal_engine("opencode").unwrap().delegate_tool,
        Some("subagent")
    );
}

#[test]
fn b791_an_engine_without_a_table_has_no_goal_loop() {
    for name in ["", "fake-acp", "pi", "Cline", "OpenCode"] {
        assert!(goal_engine(name).is_none(), "{name}");
    }
}

#[test]
fn b791_opencodes_titles_are_the_tool_name_then_the_command_and_its_input_comes_late() {
    let o = goal_engine("opencode").unwrap();
    assert_eq!(o.title, TitleStyle::NameThenCommand);
    assert!(o.title.input_is_late());
    assert!(!goal_engine("cline").unwrap().title.input_is_late());
    // the engine table the recorder reads agrees with the goal table
    assert_eq!(engine_tools("opencode").title, TitleStyle::NameThenCommand);
    assert_eq!(engine_tools("opencode").delegate, Some("subagent"));
    // a pending call is titled by the tool; a command is not a tool name
    assert!(calls_tool("opencode", "subagent", "subagent"));
    assert!(
        !calls_tool("opencode", "subagent: review", "subagent"),
        "no colon form in OpenCode"
    );
    assert_eq!(TitleStyle::NameThenCommand.tool_of("shell"), "shell");
    assert_eq!(TitleStyle::NameThenCommand.tool_of("cat hello.txt"), "");
    assert_eq!(TitleStyle::NameThenCommand.tool_of("ls -l 1"), "");
}

// ---- the limits ------------------------------------------------------------------------------------------------------------------

#[test]
fn b791_the_harness_loop_has_no_settle_wait_and_keeps_the_stall_numbers() {
    let s = settings();
    let plugin = s.limits();
    let h = harness_limits(&s, "OpenCode");
    assert_eq!(
        h.settle,
        Duration::ZERO,
        "no plugin can start a round that takes a claim's place"
    );
    assert_eq!(h.who, "OpenCode");
    assert_eq!(h.blocked_rounds, plugin.blocked_rounds);
    assert_eq!(h.stall_rounds, plugin.stall_rounds);
    assert_eq!(h.idle_rounds, plugin.idle_rounds);
}

// ---- the machine, fed the rounds the harness synthesizes -------------------------------------------------------------------------

fn round(m: &mut Machine, at: Instant, n: u64, text: &str, iterations: u32, fp: Fp) {
    m.observe(at, Obs::Start { n });
    m.observe(
        at,
        Obs::End {
            end: RoundEnd {
                n,
                status: "completed".into(),
                iterations,
                text: text.into(),
                error: None,
            },
            fp,
        },
    );
}

fn machine() -> (Machine, Instant) {
    (
        Machine::new(harness_limits(&settings(), "OpenCode")).with_base(Some("base".into())),
        Instant::now(),
    )
}

#[test]
fn b791_a_claim_in_a_round_that_ended_with_a_message_ends_the_loop_at_once() {
    let (mut m, t0) = machine();
    round(&mut m, t0, 1, "Wrote it.", 4, Fp::Is("a".into()));
    assert!(!matches!(m.decide(t0), Decision::Done { .. }));
    round(
        &mut m,
        t0,
        2,
        &format!("Checked.\n{CLAIM_WORD} {N}: ok"),
        3,
        Fp::Same,
    );
    m.observe(
        t0,
        Obs::Candidate {
            summary: Some("ok".into()),
        },
    );
    assert_eq!(
        m.decide(t0),
        Decision::Done {
            summary: Some("ok".into())
        }
    );
}

#[test]
fn b791_the_same_blocked_line_in_two_rounds_ends_the_run_with_the_harness_words() {
    let (mut m, t0) = machine();
    round(
        &mut m,
        t0,
        1,
        "BLOCKED: the API key is missing",
        2,
        Fp::Is("a".into()),
    );
    assert!(!matches!(m.decide(t0), Decision::Brake(_)));
    round(
        &mut m,
        t0,
        2,
        "BLOCKED: the API key is missing",
        2,
        Fp::Same,
    );
    let Decision::Brake(b) = m.decide(t0) else {
        panic!("no brake")
    };
    assert_eq!(b.rule, Rule::Blocked);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
    assert!(b.detail.contains("OpenCode"), "{}", b.detail);
}

#[test]
fn b791_three_rounds_with_no_tool_call_and_no_change_end_the_run() {
    let (mut m, t0) = machine();
    for n in 1..=2 {
        round(&mut m, t0, n, "Thinking about it.", 1, Fp::Same);
        assert!(!matches!(m.decide(t0), Decision::Brake(_)), "round {n}");
    }
    round(&mut m, t0, 3, "Still thinking.", 1, Fp::Same);
    let Decision::Brake(b) = m.decide(t0) else {
        panic!("no brake")
    };
    assert_eq!(b.rule, Rule::TextOnly);
    assert!(
        b.detail.starts_with("The harness ended the run:"),
        "{}",
        b.detail
    );
}

#[test]
fn b791_five_rounds_with_tool_calls_and_no_file_change_end_the_run() {
    let (mut m, t0) = machine();
    for n in 1..=4 {
        round(&mut m, t0, n, "Ran the tests again.", 5, Fp::Same);
        assert!(!matches!(m.decide(t0), Decision::Brake(_)), "round {n}");
    }
    round(&mut m, t0, 5, "Ran the tests again.", 5, Fp::Same);
    let Decision::Brake(b) = m.decide(t0) else {
        panic!("no brake")
    };
    assert_eq!(b.rule, Rule::NoProgress);
}

#[test]
fn b791_there_is_no_cap_on_rounds_that_keep_changing_files() {
    let (mut m, t0) = machine();
    for n in 1..=200u64 {
        round(
            &mut m,
            t0,
            n,
            &format!("Step {n} done."),
            6,
            Fp::Is(format!("tree-{n}")),
        );
        let d = m.decide(t0);
        assert!(
            !matches!(d, Decision::Brake(_) | Decision::Done { .. }),
            "round {n}: {d:?}"
        );
    }
    assert_eq!(m.ends(), 200);
}

#[test]
fn b791_a_round_that_changes_a_file_resets_the_stall_counters() {
    let (mut m, t0) = machine();
    round(&mut m, t0, 1, "Thinking.", 1, Fp::Same);
    round(&mut m, t0, 2, "Thinking.", 1, Fp::Same);
    round(&mut m, t0, 3, "Fixed one file.", 3, Fp::Is("b".into()));
    round(&mut m, t0, 4, "Thinking.", 1, Fp::Same);
    round(&mut m, t0, 5, "Thinking.", 1, Fp::Same);
    assert!(!matches!(m.decide(t0), Decision::Brake(_)));
}
