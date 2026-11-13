//! Calls that bring something new are not a stall for the brake inside a round.
//!
//! The brake ends a run that makes 30 tool calls in a row, or 20 minutes of calls (at least 10), with no file changed and no new text. Four runs
//! of the same ladder task (a medium feature that touches several modules) were ended that way on 2026-10-25 to 2026-10-27 while the model was
//! still reading the code to plan the change: 31 to 42 calls, every one with an input and an answer that appeared nowhere earlier in the run, none
//! of them failed, none repeated. A run that reads and plans before it edits makes no file change for as long as the task is large, and the
//! brake had no way to tell that from a loop.
//!
//! The watch now tells the two apart with what the engine puts on the wire: a call whose input is new to the run, whose answer brings at least
//! three lines that no earlier answer had, and that succeeded, does not count toward the two limits, up to an allowance (`AGENTD_GOAL_NEW_CALLS`, 100
//! between two changed files). A call that repeats an earlier one, one whose answer was seen before or differs from it in a line or two (a poll that
//! prints the time), a failed call, a call with no answer on the wire and every call beyond the allowance
//! count as before; 0 gives the old rule back.
//!
//! The first half is the pure watch with a clock the tests move, then the two functions that read the wire, then whole runs against the fake engine
//! (its scenarios `goal_explore_*` carry `rawInput` and `rawOutput` like the real engine's calls).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::goal::GoalHook;
use agentd::direct::goal::command::CommandProbe;
use agentd::direct::goal::machine::Rule;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::goal::watch::{
    Answer, Command, MIN_NEW_LINES, Watch, WatchLimits, input_key, output_key,
};
use agentd::direct::hooks::{DirectHook, Hooks};
use common::plugin_folder;
use direct_support::{Direct, Opts};
use harness_protocol::direct::{DirectOutcome, GoalOptions, HARNESS_STOP_PREFIX};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

struct Clock(Instant);

impl Clock {
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
}

fn watch(c: &Clock, allowance: u32) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into())).with_new_calls(allowance)
}

// ---- the watch ------------------------------------------------------------------------------------------------------------------

/// An answer whose lines are the given hashes, from a call that succeeded.
fn said(lines: &[u64]) -> Answer {
    Answer {
        lines: lines.to_vec(),
        ok: true,
    }
}

/// One call that began at second `at_s` and ended with the given answer (`None`: the update carried none).
fn call(w: &mut Watch, c: &Clock, n: u64, at_s: u64, input: u64, answer: Option<&Answer>) {
    let id = format!("c{n}");
    w.tool_call_keyed(c.at(at_s), &id, input);
    w.tool_done(&id, answer);
}

/// Four lines no other call of the run has: the answer of call number `n`.
fn news(n: u64) -> Answer {
    said(&[n * 10 + 1, n * 10 + 2, n * 10 + 3, n * 10 + 4])
}

/// A call whose input and answer are new: number `n` of the run.
fn fresh(w: &mut Watch, c: &Clock, n: u64, at_s: u64) {
    call(w, c, n, at_s, 1000 + n, Some(&news(n)));
}

#[test]
fn forty_calls_that_each_bring_something_new_are_not_a_stall() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..40 {
        fresh(&mut w, &c, n, 1 + n);
    }
    assert_eq!(w.look(c.at(60), Some("t0".into())), None);
    assert_eq!(w.calls_in_a_row(), 0);
    assert_eq!(w.new_credits(), 40);
}

#[test]
fn a_run_that_keeps_reading_new_things_for_longer_than_the_window_is_not_a_stall() {
    // one new read every 50 s: 25 calls over 21 minutes. TRIGGER PRESENT: the old rule ends it (the twin below).
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..25 {
        fresh(&mut w, &c, n, 1 + n * 50);
    }
    assert_eq!(w.look(c.at(21 * 60), Some("t0".into())), None);
}

#[test]
fn with_the_allowance_at_zero_the_same_reads_end_the_run_as_before() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 0);
    for n in 0..25 {
        fresh(&mut w, &c, n, 1 + n * 50);
    }
    let b = w
        .look(c.at(21 * 60), Some("t0".into()))
        .expect("25 calls over the window");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 20 minutes: no file changed and 25 of 25 tool calls counted as no progress."
        )
    );
    assert_eq!(w.new_credits(), 0);
    // and thirty in a row, with the old words
    let mut w = watch(&c, 0);
    for n in 0..30 {
        fresh(&mut w, &c, n, 1 + n);
    }
    let b = w.look(c.at(40), Some("t0".into())).expect("30 in a row");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared."
        )
    );
}

#[test]
fn the_same_call_again_is_counted_and_a_loop_of_identical_reads_ends_the_run() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    let same = said(&[7, 8, 9, 10]);
    // the first read is new; the 30 after it repeat its input and its answer
    for n in 0..30 {
        call(&mut w, &c, n, 1 + n, 7, Some(&same));
    }
    assert_eq!(
        w.look(c.at(40), Some("t0".into())),
        None,
        "30 calls, one of them new: 29 count"
    );
    call(&mut w, &c, 30, 41, 7, Some(&same));
    let b = w
        .look(c.at(46), Some("t0".into()))
        .expect("30 identical reads in a row");
    assert_eq!(b.rule, Rule::RoundNoProgress);
    assert!(b.detail.contains("30 tool calls in a row"), "{}", b.detail);
    assert_eq!(w.new_credits(), 1, "the first read earned its credit");
}

#[test]
fn a_different_command_each_time_with_the_same_answer_is_a_poll_and_ends_the_run() {
    // the A12 shape: `sleep 10 && test -e READY-n.txt`, a new input every time and "missing" every time
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    let missing = said(&[91, 92, 93, 94]);
    for n in 0..30 {
        call(&mut w, &c, n, 1 + n, 1000 + n, Some(&missing));
    }
    assert_eq!(
        w.look(c.at(40), Some("t0".into())),
        None,
        "the first answer was new"
    );
    call(&mut w, &c, 30, 41, 1030, Some(&missing));
    assert!(w.look(c.at(46), Some("t0".into())).is_some());
}

#[test]
fn a_poll_whose_answer_changes_in_one_or_two_lines_brings_nothing_new_and_ends_the_run() {
    // `sleep 30; date; ls READY.txt`: the time is a new line every time, the rest of the answer is the same
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..31u64 {
        let answer = said(&[5000 + n, 91, 92]);
        call(&mut w, &c, n, 1 + n, 1000 + n, Some(&answer));
    }
    // the first answer brought three lines (the time and two others) and was credited; no later one brought more than one
    assert_eq!(w.new_credits(), 1);
    let b = w
        .look(c.at(50), Some("t0".into()))
        .expect("31 calls, one credited");
    assert!(b.detail.contains("30 tool calls in a row"), "{}", b.detail);
}

#[test]
fn an_answer_needs_three_lines_that_no_earlier_answer_had() {
    assert_eq!(MIN_NEW_LINES, 3);
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    call(&mut w, &c, 0, 1, 1, Some(&said(&[1, 2, 3, 4])));
    assert_eq!(w.new_credits(), 1, "four new lines");
    call(&mut w, &c, 1, 2, 2, Some(&said(&[3, 4, 5, 6])));
    assert_eq!(
        w.new_credits(),
        1,
        "two of its lines were in the first answer"
    );
    call(&mut w, &c, 2, 3, 3, Some(&said(&[4, 7, 8, 9])));
    assert_eq!(w.new_credits(), 2, "three new lines are enough");
    call(&mut w, &c, 3, 4, 4, Some(&said(&[10, 11])));
    assert_eq!(w.new_credits(), 2, "two new lines are not");
    call(&mut w, &c, 4, 5, 5, Some(&said(&[])));
    assert_eq!(w.new_credits(), 2, "an answer with no lines brings nothing");
}

#[test]
fn a_call_that_failed_earns_nothing_and_a_run_that_keeps_failing_ends_with_the_old_words() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..30 {
        let failed = Answer {
            ok: false,
            ..news(n)
        };
        call(&mut w, &c, n, 1 + n, 1000 + n, Some(&failed));
    }
    let b = w.look(c.at(40), Some("t0".into())).expect("30 failures");
    assert_eq!(
        b.detail,
        format!(
            "{HARNESS_STOP_PREFIX} Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared."
        )
    );
    assert_eq!(w.new_credits(), 0);
}

#[test]
fn a_call_with_no_answer_on_the_wire_earns_nothing() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..30 {
        call(&mut w, &c, n, 1 + n, 1000 + n, None);
    }
    assert!(w.look(c.at(40), Some("t0".into())).is_some());
    assert_eq!(w.new_credits(), 0);
}

#[test]
fn a_call_with_no_key_is_counted_as_before() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    for n in 0..30 {
        w.tool_call(c.at(1 + n));
    }
    assert!(w.look(c.at(40), Some("t0".into())).is_some());
}

#[test]
fn the_allowance_is_bounded_and_the_calls_beyond_it_count() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 5);
    for n in 0..34 {
        fresh(&mut w, &c, n, 1 + n);
    }
    assert_eq!(w.new_credits(), 5);
    assert_eq!(w.calls_in_a_row(), 29);
    assert_eq!(w.look(c.at(40), Some("t0".into())), None);
    fresh(&mut w, &c, 34, 41);
    fresh(&mut w, &c, 35, 42);
    let b = w
        .look(c.at(50), Some("t0".into()))
        .expect("36 calls, 5 free");
    assert!(
        b.detail.contains("in 31 tool calls in a row"),
        "{}",
        b.detail
    );
    assert_eq!(w.new_credits(), 5, "five reads earned their credit");
}

#[test]
fn a_changed_file_gives_the_allowance_back() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 10);
    for n in 0..10 {
        fresh(&mut w, &c, n, 1 + n);
    }
    assert_eq!(w.new_credits(), 10);
    // the tree changes: both counts and the allowance start again
    assert_eq!(w.look(c.at(20), Some("t1".into())), None);
    assert_eq!(w.new_credits(), 0);
    for n in 10..20 {
        fresh(&mut w, &c, n, 30 + n);
    }
    assert_eq!(w.new_credits(), 10);
    assert_eq!(w.calls_in_a_row(), 0);
}

#[test]
fn an_answer_to_a_call_that_began_before_a_change_of_the_tree_earns_nothing() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    w.tool_call_keyed(c.at(1), "old", 1);
    assert_eq!(
        w.look(c.at(10), Some("t1".into())),
        None,
        "the tree changed"
    );
    w.tool_call(c.at(11));
    assert_eq!(w.calls_in_a_row(), 1);
    w.tool_done("old", Some(&news(2)));
    assert_eq!(w.new_credits(), 0);
    assert_eq!(
        w.calls_in_a_row(),
        1,
        "the late answer does not take back a call of the new stretch"
    );
}

#[test]
fn an_answer_nobody_asked_for_and_a_second_answer_earn_nothing() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    w.tool_done("nobody", Some(&news(99)));
    assert_eq!(w.new_credits(), 0);
    w.tool_call_keyed(c.at(1), "a", 1);
    w.tool_done("a", Some(&news(2)));
    w.tool_done("a", Some(&news(3)));
    assert_eq!(w.new_credits(), 1);
    assert_eq!(w.calls_in_a_row(), 0);
}

#[test]
fn a_call_that_has_no_room_among_the_pending_ones_earns_nothing() {
    // the watch keeps 1024 calls that wait for an answer (UNFOLDED_MAX), as it keeps 1024 arrivals for a sample of the evidence
    let c = Clock(Instant::now());
    let mut w = watch(&c, 5000);
    for n in 0..1030u64 {
        w.tool_call_keyed(c.at(1), &format!("p{n}"), 10_000 + n);
    }
    w.tool_done("p1029", Some(&news(1)));
    assert_eq!(w.new_credits(), 0, "no room was kept for it");
    w.tool_done("p0", Some(&news(2)));
    assert_eq!(w.new_credits(), 1);
}

#[test]
fn a_call_a_running_command_took_back_is_not_credited_a_second_time() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    w.tool_call_keyed(c.at(1), "p", 1);
    assert_eq!(w.calls_in_a_row(), 1);
    // the evidence says a job older than the call was running: the call is a wait for it and is taken back
    w.command(
        c.at(100),
        Command::Active {
            age: Duration::from_secs(3600),
        },
    );
    assert_eq!(w.calls_in_a_row(), 0);
    w.tool_call(c.at(101));
    assert_eq!(w.calls_in_a_row(), 1);
    w.tool_done("p", Some(&news(2)));
    assert_eq!(
        w.calls_in_a_row(),
        1,
        "taken back once; its answer takes nothing more"
    );
    assert_eq!(w.new_credits(), 0);
}

#[test]
fn a_credited_call_is_left_out_of_the_calls_a_running_command_may_take_back() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 100);
    w.tool_call(c.at(1)); // counted
    w.tool_call_keyed(c.at(2), "n", 1); // counted, then credited
    w.tool_done("n", Some(&news(2)));
    assert_eq!(w.calls_in_a_row(), 1);
    w.command(
        c.at(100),
        Command::Active {
            age: Duration::from_secs(3600),
        },
    );
    assert_eq!(
        w.calls_in_a_row(),
        0,
        "the one call that was still counted is taken back, and the credited one is not taken back too"
    );
}

// ---- what the wire says ---------------------------------------------------------------------------------------------------------

fn call_update(id: &str, kind: &str, title: &str, input: Value) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": kind, "title": title, "status": "pending", "rawInput": input})
}

fn answer(id: &str, status: &str, out: Value) -> Value {
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "rawOutput": out})
}

/// The shell tool's answer: `[{"query": <the command>, "result": <what it printed>, "success": true}]`.
fn shell(id: &str, command: &str, printed: &str) -> Value {
    answer(
        id,
        "completed",
        json!([{"query": command, "result": printed, "success": true}]),
    )
}

#[test]
fn the_key_of_a_call_is_what_it_asked_for_and_not_its_id() {
    let a = call_update(
        "tooluse_1",
        "execute",
        "run_commands: rg -n x",
        json!({"commands": ["rg -n x"]}),
    );
    let b = call_update(
        "tooluse_2",
        "execute",
        "run_commands: rg -n x",
        json!({"commands": ["rg -n x"]}),
    );
    let other = call_update(
        "tooluse_3",
        "execute",
        "run_commands: rg -n y",
        json!({"commands": ["rg -n y"]}),
    );
    assert!(input_key(&a).is_some());
    assert_eq!(input_key(&a), input_key(&b));
    assert_ne!(input_key(&a), input_key(&other));
    // another kind of call with the same words is another ask
    let read = call_update(
        "tooluse_4",
        "read",
        "run_commands: rg -n x",
        json!({"commands": ["rg -n x"]}),
    );
    assert_ne!(input_key(&a), input_key(&read));
}

#[test]
fn a_call_with_no_input_is_keyed_by_its_title_and_one_with_neither_has_no_key() {
    let bare = |id: &str, title: &str| json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "other", "title": title, "status": "pending"});
    assert!(input_key(&bare("a", "list_files: src")).is_some());
    assert_eq!(
        input_key(&bare("a", "list_files: src")),
        input_key(&bare("b", "list_files: src"))
    );
    assert_ne!(
        input_key(&bare("a", "list_files: src")),
        input_key(&bare("a", "list_files: docs"))
    );
    assert_eq!(
        input_key(&json!({"sessionUpdate": "tool_call", "toolCallId": "z", "status": "pending"})),
        None
    );
}

#[test]
fn the_lines_of_an_answer_are_the_distinct_non_blank_lines_of_what_came_back() {
    let a = output_key(&shell("1", "cat x", "one\ntwo\n\n   \nthree\n")).expect("an answer");
    assert!(a.ok);
    assert_eq!(a.lines.len(), 3, "blank lines are no lines");
    // the same text is the same lines, whatever the call's id or the command that printed it
    let b = output_key(&shell("2", "cat y", "one\ntwo\n\n   \nthree\n")).unwrap();
    assert_eq!(a.lines, b.lines);
    // a line twice in one answer is one line
    let twice = output_key(&shell("3", "yes | head", "same\nsame\nsame\nother\n")).unwrap();
    assert_eq!(twice.lines.len(), 2);
    // another text is other lines; one shared line is shared
    let c = output_key(&shell("4", "cat z", "one\nfour\nfive\n")).unwrap();
    let shared: Vec<_> = c.lines.iter().filter(|l| a.lines.contains(l)).collect();
    assert_eq!(shared.len(), 1);
    // every string of the answer counts: two results of one call
    let two = output_key(&answer(
        "5",
        "completed",
        json!([{"query": "a", "result": "x1\nx2", "success": true}, {"query": "b", "result": "y1", "success": true}]),
    ))
    .unwrap();
    assert_eq!(two.lines.len(), 3);
}

#[test]
fn the_lines_of_an_answer_leave_out_the_engines_echo_of_the_call() {
    // The engine's shell tool answers `[{"query": <the command>, "result": <what it printed>, "success": true}]`: the command is the call's own
    // words, not something the call brought. A poll that words its check differently each time and gets "missing" each time has one answer.
    let a = output_key(&shell(
        "1",
        "sleep 10 && test -e READY-1.txt",
        "missing\nno file\nretry\n",
    ))
    .unwrap();
    let b = output_key(&shell(
        "2",
        "sleep 10 && test -e READY-2.txt",
        "missing\nno file\nretry\n",
    ))
    .unwrap();
    let c = output_key(&shell(
        "3",
        "sleep 10 && test -e READY-3.txt",
        "found\nfile\nnow\n",
    ))
    .unwrap();
    assert_eq!(a.lines, b.lines);
    assert_ne!(a.lines, c.lines);
    // the echo is left out at any depth, but only under that name
    let nested = output_key(&answer(
        "4",
        "completed",
        json!({"content": [{"query": "x", "text": "t1\nt2"}]}),
    ))
    .unwrap();
    let other = output_key(&answer(
        "5",
        "completed",
        json!({"content": [{"query": "y", "text": "t1\nt2"}]}),
    ))
    .unwrap();
    let named = output_key(&answer(
        "6",
        "completed",
        json!({"content": [{"title": "y", "text": "t1\nt2"}]}),
    ))
    .unwrap();
    assert_eq!(nested.lines, other.lines);
    assert_eq!(nested.lines.len(), 2);
    assert_eq!(named.lines.len(), 3);
}

#[test]
fn an_answer_that_failed_says_so_in_each_of_the_ways_the_engine_says_it() {
    let failed_status = answer("1", "failed", json!("boom"));
    assert_eq!(output_key(&failed_status).map(|k| k.ok), Some(false));
    let unsuccessful = answer(
        "2",
        "completed",
        json!([{"query": "q", "result": "cat: x: No such file", "success": false}]),
    );
    assert_eq!(output_key(&unsuccessful).map(|k| k.ok), Some(false));
    let with_error = answer(
        "3",
        "completed",
        json!([{"query": "q", "result": "", "success": true, "error": "exit 1"}]),
    );
    assert_eq!(output_key(&with_error).map(|k| k.ok), Some(false));
    let is_error = answer(
        "4",
        "completed",
        json!({"content": [{"type": "text", "text": "no"}], "isError": true}),
    );
    assert_eq!(output_key(&is_error).map(|k| k.ok), Some(false));
    let one_of_two = answer(
        "5",
        "completed",
        json!([{"result": "a", "success": true}, {"result": "b", "success": false}]),
    );
    assert_eq!(output_key(&one_of_two).map(|k| k.ok), Some(false));
    let fine = shell("6", "ls", "a\nb\n");
    assert_eq!(output_key(&fine).map(|k| k.ok), Some(true));
}

#[test]
fn an_update_that_is_not_an_answer_has_no_key() {
    assert!(
        output_key(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"})
        )
        .is_none(),
        "no result on the wire"
    );
    assert!(output_key(&answer("1", "in_progress", json!("partial"))).is_none());
    assert!(output_key(&answer("1", "pending", json!("x"))).is_none());
    assert!(output_key(&answer("1", "completed", Value::Null)).is_none());
}

// ---- the settings ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_member_of_the_request_replaces_the_stage_allowance_for_one_run() {
    let stage =
        GoalSettings::from_lookup(&|k| (k == "AGENTD_GOAL_NEW_CALLS").then(|| "40".to_string()));
    assert_eq!(stage.new_calls, 40);
    let with = |n: u32| {
        stage
            .with_request(&GoalOptions {
                new_calls: Some(n),
                ..GoalOptions::switch(true)
            })
            .new_calls
    };
    assert_eq!(with(7), 7);
    assert_eq!(with(0), 0, "0 is the request's word for count every call");
    assert_eq!(with(5000), 1000, "cut to the bound again");
    assert_eq!(stage.with_request(&GoalOptions::switch(true)).new_calls, 40);
    // the stage's own settings are untouched, and the watch is built from the run's
    assert_eq!(stage.new_calls, 40);
}

// ---- whole runs ---------------------------------------------------------------------------------------------------------------

/// What the evidence of a running command says: nothing runs, so no call is a wait for a job.
#[derive(Debug)]
struct Quiet;

impl CommandProbe for Quiet {
    fn rebaseline(&self) {}
    fn sample(&self, _now: Instant) -> Command {
        Command::Idle
    }
}

/// A plugin folder like the image's and times short enough to run in seconds: a look at the tree every 50 ms, a window of 10 s.
fn settings(extra: &[(&str, &str)]) -> GoalSettings {
    let mut m: BTreeMap<String, String> = [
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
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50".to_string()),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "10".to_string()),
        ("AGENTD_GOAL_ROUND_MIN_CALLS", "3".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        m.insert((*k).to_string(), (*v).to_string());
    }
    GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

async fn start(scenario: &'static str) -> Direct {
    Direct::start(Opts {
        scenario,
        timeout_s: 90,
        ..Opts::default()
    })
    .await
}

async fn run(d: &Direct, s: GoalSettings) -> InvocationResponse {
    let goal = GoalHook::for_engine_with_probe("cline", s, Arc::new(Quiet))
        .expect("the plugin folder is in the repository");
    let deps = Deps {
        hooks: Some(Hooks::new(vec![goal as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn phases(d: &Direct) -> Vec<String> {
    d.aws
        .progress("goal-loop")
        .into_iter()
        .map(|(_, v)| v["phase"].as_str().unwrap().to_string())
        .collect()
}

/// The run was ended by the brake inside a round: the outcome, the label and the file the model never got to write.
fn assert_braked(d: &Direct, resp: &InvocationResponse) -> String {
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}\n{:?}",
        d.engine_trace(),
        phases(d)
    );
    let s = d.summary();
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedOther,
        "{}\n{:?}",
        d.engine_trace(),
        phases(d)
    );
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run (Cline made no progress in "),
        "{}",
        s.label
    );
    assert!(phases(d).contains(&"brake".to_string()), "{:?}", phases(d));
    assert!(
        !d.worktree().join("notes/result.txt").exists(),
        "the model never got to its edit"
    );
    s.label
}

#[tokio::test(flavor = "multi_thread")]
async fn forty_distinct_reads_and_then_an_edit_are_not_stopped() {
    let d = start("goal_explore_distinct").await;
    let t0 = Instant::now();
    let resp = run(&d, settings(&[])).await;
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
        DirectOutcome::ReportedDone,
        "{}\n{}",
        s.label,
        d.engine_trace()
    );
    // TRIGGER PRESENT: 40 reads over 12 seconds with no file changed until the end: more than 30 calls in a row, more than the window of 10 s
    // and its 3 calls; the twin below is ended by the old rule
    assert!(s.tool_calls >= 40, "{}", s.tool_calls);
    assert!(t0.elapsed() > Duration::from_secs(10), "{:?}", t0.elapsed());
    assert!(!phases(&d).contains(&"brake".to_string()));
    assert!(d.worktree().join("notes/result.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_forty_reads_with_the_allowance_at_zero_end_the_run_as_before() {
    let d = start("goal_explore_distinct").await;
    let resp = run(&d, settings(&[("AGENTD_GOAL_NEW_CALLS", "0")])).await;
    let label = assert_braked(&d, &resp);
    assert!(!label.contains("brought something new"), "{label}");
    assert!(d.summary().tool_calls >= 10, "{}", d.summary().tool_calls);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_read_forty_times_ends_the_run() {
    let d = start("goal_explore_repeat").await;
    let resp = run(&d, settings(&[])).await;
    let label = assert_braked(&d, &resp);
    assert!(label.contains("tool calls in a row"), "{label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_check_each_time_with_the_same_answer_ends_the_run() {
    let d = start("goal_explore_polls").await;
    let resp = run(&d, settings(&[])).await;
    let label = assert_braked(&d, &resp);
    assert!(label.contains("tool calls in a row"), "{label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_poll_that_prints_the_time_ends_the_run() {
    // `sleep 30; date; ls READY.txt`: a new command and a new line (the time) in every answer, but never three lines that are new
    let d = start("goal_explore_timestamps").await;
    let resp = run(&d, settings(&[])).await;
    let label = assert_braked(&d, &resp);
    assert!(label.contains("tool calls in a row"), "{label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_whose_calls_keep_failing_ends_with_the_old_words() {
    let d = start("goal_explore_failing").await;
    let resp = run(&d, settings(&[])).await;
    let label = assert_braked(&d, &resp);
    assert!(label.contains("tool calls in a row"), "{label}");
    assert!(!label.contains("brought something new"), "{label}");
}
