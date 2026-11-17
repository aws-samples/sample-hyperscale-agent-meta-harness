// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! The settings of the brake inside a round and of its final-message turn: each is a stage setting (`AGENTD_GOAL_*`) with
//! the default of the decision record, bounded by the same constants the protocol check uses, and each can be replaced for one run by a member of
//! the request's `goal` object (`ham` sends it when the user sets the key).
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::settings::{
    DEFAULT_GOAL_TEXT, GoalSettings, goal_text_in, strip_outside_note,
};
use harness_protocol::direct::{
    GOAL_COMMAND_GRACE_S, GOAL_ROUND_CALLS, GOAL_ROUND_MIN_CALLS, GOAL_ROUND_WINDOW_S, GoalOptions,
};
use std::collections::BTreeMap;
use std::time::Duration;

fn from(pairs: &[(&str, &str)]) -> GoalSettings {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

#[test]
fn the_defaults_are_the_numbers_the_protocol_names() {
    let s = from(&[]);
    assert_eq!(s.round_calls, GOAL_ROUND_CALLS.default);
    assert_eq!(
        s.round_window,
        Duration::from_secs(u64::from(GOAL_ROUND_WINDOW_S.default))
    );
    assert_eq!(s.round_min_calls, GOAL_ROUND_MIN_CALLS.default);
    assert_eq!(s.command_grace, Duration::from_secs(7200));
    assert_eq!(
        s.command_grace,
        Duration::from_secs(u64::from(GOAL_COMMAND_GRACE_S.default))
    );
    assert!(s.final_message);
    assert_eq!(s.final_window, Duration::from_secs(120));
    assert!(s.command_evidence);
    assert_eq!(s.command_cpu_pct, 5);
}

#[test]
fn the_stage_settings_are_read_and_cut_to_their_bounds() {
    let s = from(&[
        ("AGENTD_GOAL_COMMAND_GRACE_S", "90"),
        ("AGENTD_GOAL_FINAL_S", "45"),
        ("AGENTD_GOAL_COMMAND_CPU_PCT", "12"),
    ]);
    assert_eq!(
        (s.command_grace, s.final_window, s.command_cpu_pct),
        (Duration::from_secs(90), Duration::from_secs(45), 12)
    );
    let low = from(&[
        ("AGENTD_GOAL_COMMAND_GRACE_S", "10"),
        ("AGENTD_GOAL_FINAL_S", "1"),
        ("AGENTD_GOAL_COMMAND_CPU_PCT", "0"),
    ]);
    assert_eq!(low.command_grace, Duration::from_secs(60));
    assert_eq!(low.final_window, Duration::from_secs(5));
    assert_eq!(low.command_cpu_pct, 1);
    let high = from(&[
        ("AGENTD_GOAL_COMMAND_GRACE_S", "999999"),
        ("AGENTD_GOAL_FINAL_S", "99999"),
        ("AGENTD_GOAL_COMMAND_CPU_PCT", "400"),
    ]);
    assert_eq!(high.command_grace, Duration::from_secs(86_400));
    assert_eq!(high.final_window, Duration::from_secs(600));
    assert_eq!(high.command_cpu_pct, 100);
    let junk = from(&[("AGENTD_GOAL_COMMAND_GRACE_S", "two hours")]);
    assert_eq!(
        junk.command_grace,
        Duration::from_secs(7200),
        "a value that does not parse is the default"
    );
}

#[test]
fn the_switches_read_off_the_way_the_loop_switch_does() {
    for off in ["off", "0", "false", "no", "disabled", " OFF "] {
        let s = from(&[("AGENTD_GOAL_FINAL", off), ("AGENTD_GOAL_COMMAND", off)]);
        assert!(!s.final_message, "{off}");
        assert!(!s.command_evidence, "{off}");
    }
    let on = from(&[("AGENTD_GOAL_FINAL", "yes"), ("AGENTD_GOAL_COMMAND", "on")]);
    assert!(on.final_message && on.command_evidence);
}

#[test]
fn a_member_of_the_request_replaces_the_stage_setting_for_the_run() {
    let stage = from(&[
        ("AGENTD_GOAL_ROUND_CALLS", "50"),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "600"),
    ]);
    let run = stage.with_request(&GoalOptions {
        round_calls: Some(45),
        command_grace_s: Some(600),
        final_message: Some(false),
        ..GoalOptions::switch(true)
    });
    assert_eq!(run.round_calls, 45, "the request wins");
    assert_eq!(run.command_grace, Duration::from_secs(600));
    assert!(!run.final_message);
    assert_eq!(
        run.round_window,
        Duration::from_secs(600),
        "a member the request leaves out keeps the stage value"
    );
    assert_eq!(run.round_min_calls, 10);
    let limits = run.watch_limits("Cline");
    assert_eq!(
        (limits.calls, limits.window, limits.min_calls),
        (45, Duration::from_secs(600), 10)
    );
    // and the stage's own settings are untouched
    assert_eq!(stage.round_calls, 50);
    assert!(stage.final_message);
}

#[test]
fn a_grace_of_zero_stops_the_look_for_running_commands() {
    // on the stage, and for one run through the request
    let stage = from(&[("AGENTD_GOAL_COMMAND_GRACE_S", "0")]);
    assert_eq!(stage.command_grace, Duration::ZERO);
    assert!(stage.command_evidence && !stage.looks_for_commands());
    assert!(from(&[]).looks_for_commands());
    let run = from(&[]).with_request(&GoalOptions {
        command_grace_s: Some(0),
        ..GoalOptions::switch(true)
    });
    assert!(!run.looks_for_commands());
    assert!(!from(&[("AGENTD_GOAL_COMMAND", "off")]).looks_for_commands());
}

#[test]
fn a_plain_switch_changes_nothing() {
    let stage = from(&[("AGENTD_GOAL_ROUND_CALLS", "50")]);
    assert_eq!(stage.with_request(&GoalOptions::switch(true)), stage);
}

// ---- the allowance of calls that bring something new (AGENTD_GOAL_NEW_CALLS) -----------------------------------------------------

#[test]
fn the_allowance_of_calls_that_bring_something_new_defaults_to_100() {
    assert_eq!(from(&[]).new_calls, 100);
}

#[test]
fn the_allowance_is_read_cut_to_its_bounds_and_zero_or_an_off_word_counts_every_call() {
    let get = |v: &str| from(&[("AGENTD_GOAL_NEW_CALLS", v)]).new_calls;
    assert_eq!(get("250"), 250);
    assert_eq!(get(" 7 "), 7);
    assert_eq!(get("1"), 1);
    assert_eq!(get("1000"), 1000);
    assert_eq!(get("99999"), 1000, "cut to the upper bound");
    for off in ["0", "off", "OFF", "false", "no", "disabled"] {
        assert_eq!(get(off), 0, "{off}");
    }
    for junk in ["", "many", "-3", "1.5", "100 calls"] {
        assert_eq!(
            get(junk),
            100,
            "a value that does not parse is the default: {junk:?}"
        );
    }
}

/// COMPAT-CAH-DATA: the default goal text names the records folder; in a run that works in the earlier folder it names that one, so the plugin's
/// follow-up prompts agree with the first line of the first prompt. A text the user wrote is left as it is.
#[test]
fn the_default_goal_text_names_the_folder_the_run_works_in() {
    assert_eq!(goal_text_in(".ham", DEFAULT_GOAL_TEXT), DEFAULT_GOAL_TEXT);
    let earlier = goal_text_in(".cah", DEFAULT_GOAL_TEXT);
    assert!(
        earlier.starts_with("Finish the task you were given. If this task has a lifecycle goal card (.cah/lifecycle/GOAL.md, or the same path with the folder that .cah/records names in place of .cah), its numbered checks are the goal"),
        "{earlier}"
    );
    assert!(!earlier.contains(".ham"), "{earlier}");
    // the rest of the text is the default's
    assert_eq!(
        earlier.split_once("its numbered checks").unwrap().1,
        DEFAULT_GOAL_TEXT
            .split_once("its numbered checks")
            .unwrap()
            .1
    );
    // without the sentence about work outside the checkout it is the same rule
    let stripped = strip_outside_note(DEFAULT_GOAL_TEXT);
    assert_eq!(
        goal_text_in(".cah", &stripped),
        stripped.replace(".ham", ".cah")
    );
    // a text the user wrote is the user's
    let own = "Make every test in tests/ pass; the card is .ham/lifecycle/GOAL.md.";
    assert_eq!(goal_text_in(".cah", own), own);
}

/// The fixed sentence of the goal loop names the goal card of THIS task. "If the lifecycle goal card exists" pointed at any card in the
/// repository, also the card an earlier request left there, and the plugin quotes the sentence in every follow-up prompt.
#[test]
fn b857_the_default_goal_text_names_this_tasks_goal_card_and_not_any_card_in_the_repository() {
    let t = DEFAULT_GOAL_TEXT;
    assert!(
        t.starts_with(
            "Finish the task you were given. If this task has a lifecycle goal card (.ham/lifecycle/GOAL.md, or the same path with the folder that \
             .ham/records names in place of .ham), its numbered checks are the goal: every check must pass in a run made after your last edit."
        ),
        "{t}"
    );
    assert!(!t.contains("goal card exists"), "{t}");
    // the words `goal_text_in` splits on stay, once, so a run in another records folder still reads its own folder's name
    assert_eq!(t.matches(" its numbered checks").count(), 1, "{t}");
    let head = t.split_once(" its numbered checks").unwrap().0;
    assert!(head.ends_with("in place of .ham),"), "{head}");
    // the plugin quotes the text in every follow-up prompt: it stays short
    assert!(t.len() < 700, "{}", t.len());
}

/// The follow-up prompts of the goal loop name the goal card of THIS task as well. "Run the checks of the goal card again" stood beside the
/// goal text in every follow-up prompt and named whatever card the repository holds, also the card an earlier request left there: a task with no
/// card of its own was told two things in every round.
#[test]
fn b857_the_follow_up_prompts_name_this_tasks_goal_card_and_only_if_it_has_one() {
    use agentd::direct::goal::harness::{continue_prompt, verification_prompt};
    use agentd::direct::goal::{KICK_PROMPT, kick_prompt};
    let prompts = [
        verification_prompt(DEFAULT_GOAL_TEXT, "N0NCE", true),
        continue_prompt(DEFAULT_GOAL_TEXT, "N0NCE", true),
        KICK_PROMPT.to_string(),
        kick_prompt(false),
    ];
    for p in &prompts {
        assert!(
            p.contains("Run the checks of this task's goal card again"),
            "{p}"
        );
        assert!(
            p.contains(", if it has one, and answer from their output"),
            "{p}"
        );
        assert!(!p.contains("of the goal card"), "{p}");
    }
    // the continue prompt of a loop that went quiet stays short
    assert!(KICK_PROMPT.len() < 400, "{}", KICK_PROMPT.len());
}
