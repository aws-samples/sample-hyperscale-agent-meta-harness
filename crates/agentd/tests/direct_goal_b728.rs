//! The brake inside a round holds back while the engine waits for its helper agents.
//!
//! In the release gate of 2026-11-01 four of six runs delivered complete code and were then ended by the brake 18 to 21 minutes into the
//! lifecycle review: the engine had started three to eight read-only reviewers (its helper-agent tool), waited for them, and they read files
//! for longer than the 20-minute window while the tree could not change. Their calls arrive on the same stream as the engine's own with nothing
//! that names the agent, and the second and third reviewer read the files the first had read, so "something new" per run is the wrong
//! measure for them. While a helper-agent call is open the brake pauses the clock and counts no call; the hold is bounded by the grace and by a
//! repetition limit, and the clock goes on from the time that was not protected when the helpers return.
//!
//! The first half replays the recorded calls of those runs (structure only: which call, how long, how many lines were new, what repeated) through
//! the same code the module feeds the wire through; the second half is the pure watch with a clock the tests move.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::machine::Rule;
use agentd::direct::goal::watch::{Command, Watch, WatchLimits, WireTools, feed_update};
use agentd::engine::tools::{TitleStyle, calls_tool};
use harness_protocol::direct::{DirectOutcome, HARNESS_STOP_PREFIX};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01M6C3D78J6WJM6ADMEPMRMJ38";

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

fn watch(c: &Clock) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into()))
        .with_new_calls(100)
        .with_next_step(format!("Next: ham pull {RUN} (files)."))
}

fn tools(delegate: bool) -> WireTools<'static> {
    WireTools {
        completion: "mark_goal_complete",
        delegate: delegate.then_some("spawn_agent"),
        title: TitleStyle::NameColonArgs,
    }
}

// ---- replay of the recorded runs --------------------------------------------------------------------------------------------------

const TRACES: [(&str, &str); 4] = [
    ("web1", include_str!("b728_traces/web1.json")),
    ("web2", include_str!("b728_traces/web2.json")),
    ("events2", include_str!("b728_traces/events2.json")),
    ("ent2", include_str!("b728_traces/ent2.json")),
];
const DONE_TRACE: &str = include_str!("b728_traces/ent1.json");

/// One event of a recorded stretch: the second, the order among events of the same second, and the update the engine sent.
fn events(trace: &Value) -> Vec<(u64, u8, Value)> {
    let mut out = Vec::new();
    // The run saw 6,000 distinct lines before the stretch began: the lines a call "re-reads" are among them.
    out.push((
        0,
        0,
        json!({"sessionUpdate": "tool_call", "toolCallId": "pre", "kind": "read", "title": "read", "status": "pending", "rawInput": {"i": 0}}),
    ));
    let old: Vec<String> = (0..6000).map(|i| format!("old-{i}")).collect();
    out.push((
        0,
        1,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "pre", "status": "completed", "rawOutput": [{"query": "q", "result": old.join("\n"), "success": true}]}),
    ));
    for call in trace["calls"].as_array().unwrap() {
        let id = call["id"].as_str().unwrap();
        let helper = call["helper"].as_bool().unwrap_or(false);
        let kind = if helper {
            "think"
        } else {
            call["kind"].as_str().unwrap()
        };
        let title = if helper {
            "spawn_agent: review".to_string()
        } else {
            kind.to_string()
        };
        out.push((
            call["t"].as_u64().unwrap(),
            2,
            json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": kind, "title": title, "status": "pending", "rawInput": {"i": call["in"]}}),
        ));
        let Some(end) = call["end"].as_u64() else {
            continue;
        };
        let fail = call["fail"].as_bool().unwrap_or(false);
        let status = if fail { "failed" } else { "completed" };
        let update = if call["noout"].as_bool().unwrap_or(false) {
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status})
        } else {
            let lines = call["lines"].as_u64().unwrap();
            let new = call["new"].as_u64().unwrap();
            let mut text: Vec<String> = (0..lines - new).map(|i| format!("old-{i}")).collect();
            text.extend((0..new).map(|i| format!("{id}-fresh-{i}")));
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "rawOutput": [{"query": "q", "result": text.join("\n"), "success": !fail}]})
        };
        out.push((end, 3, update));
    }
    for t in trace["texts"].as_array().unwrap() {
        out.push((
            t.as_u64().unwrap(),
            4,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "words"}}),
        ));
    }
    out.sort_by_key(|(t, o, _)| (*t, *o));
    out
}

/// Plays a recorded stretch with an unchanged tree (the idle session: no CPU readings) and looks at it every 5 seconds, the beat of the module,
/// until `until` seconds. Returns the second of the brake and its words.
fn replay(trace: &Value, delegate: bool, until: u64) -> Option<(u64, String)> {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    let ev = events(trace);
    let mut next = 0;
    for s in 0..=until {
        while next < ev.len() && ev[next].0 <= s {
            feed_update(&mut w, c.at(s), &ev[next].2, tools(delegate));
            next += 1;
        }
        if s % 5 == 0
            && w.wants_look(c.at(s), Duration::from_secs(5))
            && let Some(b) = w.look(c.at(s), Some("t0".into()))
        {
            assert_eq!(b.rule, Rule::RoundNoProgress);
            return Some((s, b.detail));
        }
    }
    None
}

fn trace(src: &str) -> Value {
    serde_json::from_str(src).unwrap()
}

#[test]
fn b728_replay_without_the_hold_ends_each_recorded_run_before_its_reviewers_return() {
    // TRIGGER PRESENT: the old rule ended these four runs with the code complete and the reviewers still reading.
    for (name, src) in TRACES {
        let t = trace(src);
        let stop = t["stopAt"].as_u64().unwrap();
        let (at, detail) = replay(&t, false, stop + 600)
            .unwrap_or_else(|| panic!("{name}: the old rule ends the run"));
        assert!(
            (1200..=stop + 5).contains(&at),
            "{name}: ended at {at} s, recorded end {stop} s"
        );
        assert!(detail.starts_with(HARNESS_STOP_PREFIX), "{name}: {detail}");
    }
}

#[test]
fn b728_replay_with_the_hold_does_not_end_any_recorded_run_while_the_reviewers_work() {
    for (name, src) in TRACES {
        let t = trace(src);
        let stop = t["stopAt"].as_u64().unwrap();
        // Through the recorded end, and for ten minutes after the reviewers were answered.
        let got = replay(&t, true, stop + 600);
        assert_eq!(got, None, "{name}");
    }
}

#[test]
fn b728_the_hold_never_ends_a_recorded_run_sooner_than_the_old_rule_did() {
    // The run that reported done had one reviewer open for 10 minutes. Without the CPU readings that excused it in the live run the old rule ends it
    // at the window; with the hold it is ended no sooner.
    let t = trace(DONE_TRACE);
    let stop = t["stopAt"].as_u64().unwrap();
    let old = replay(&t, false, stop)
        .expect("TRIGGER PRESENT: the old rule ends the replay at the window")
        .0;
    assert!((1200..1300).contains(&old), "{old}");
    if let Some((new, _)) = replay(&t, true, stop) {
        assert!(new >= old, "{new} before {old}");
    }
}

#[test]
fn b728_the_engine_names_its_helper_agent_tool_in_its_own_words() {
    let g = agentd::engine::goal::goal_engine("cline").unwrap();
    assert_eq!(g.delegate_tool, Some("spawn_agent"));
    assert!(calls_tool(
        "cline",
        "spawn_agent: review the tests",
        "spawn_agent"
    ));
    assert!(calls_tool("cline", "spawn_agent", "spawn_agent"));
    assert!(!calls_tool("cline", "spawn_agents: x", "spawn_agent"));
    assert!(!calls_tool("cline", "read: spawn_agent", "spawn_agent"));
    assert_eq!(g.title, TitleStyle::NameColonArgs);
}

// ---- the watch ---------------------------------------------------------------------------------------------------------------------

fn open_helper(w: &mut Watch, c: &Clock, id: &str, at: u64) {
    feed_update(
        w,
        c.at(at),
        &json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "think", "title": "spawn_agent: review", "status": "pending"}),
        tools(true),
    );
}

fn answer_helper(w: &mut Watch, c: &Clock, id: &str, at: u64) {
    feed_update(
        w,
        c.at(at),
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "rawOutput": "findings"}),
        tools(true),
    );
}

fn call(w: &mut Watch, c: &Clock, id: &str, input: u64, at: u64) {
    feed_update(
        w,
        c.at(at),
        &json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "read", "title": "read", "status": "pending", "rawInput": {"i": input}}),
        tools(true),
    );
}

#[test]
fn b728_calls_that_arrive_while_a_helper_works_are_not_counted_and_not_remembered() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    open_helper(&mut w, &c, "h1", 10);
    for i in 0..100u64 {
        call(&mut w, &c, &format!("c{i}"), 1000 + i, 20 + i);
    }
    assert_eq!(w.calls_in_a_row(), 0);
    assert_eq!(w.delegates_open(), 1);
    assert_eq!(w.look(c.at(200), Some("t0".into())), None);
    answer_helper(&mut w, &c, "h1", 210);
    assert_eq!(w.delegates_open(), 0);
    // The engine's own agent reads one of the files a helper read: the input is new to the run, so it still earns its credit.
    call(&mut w, &c, "own", 1000, 220);
    assert_eq!(w.calls_in_a_row(), 1);
}

#[test]
fn b728_the_clock_goes_on_from_the_time_that_was_not_protected_when_the_helpers_return() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    // 12 minutes of ordinary calls with an unchanged tree (12 counted calls), then a helper for 20 minutes, then ordinary calls again.
    for i in 0..12u64 {
        w.tool_call(c.at(1 + i * 60));
    }
    open_helper(&mut w, &c, "h1", 720);
    assert_eq!(
        w.look(c.at(1500), Some("t0".into())),
        None,
        "20 minutes of helper time are paused"
    );
    answer_helper(&mut w, &c, "h1", 1920);
    // 32 minutes have passed, 20 of them protected: 12 are left to count, so 8 more minutes are needed, not 20 again.
    w.tool_call(c.at(1925));
    assert_eq!(
        w.look(c.at(2000), Some("t0".into())),
        None,
        "12 minutes and a bit are not the window"
    );
    let b = w
        .look(c.at(2450), Some("t0".into()))
        .expect("the window is full again");
    assert!(
        b.detail
            .contains("no file changed and 13 of 13 tool calls counted as no progress"),
        "{}",
        b.detail
    );
}

#[test]
fn b728_helpers_that_overlap_are_protected_once() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    for i in 0..10u64 {
        w.tool_call(c.at(1 + i));
    }
    open_helper(&mut w, &c, "a", 20);
    open_helper(&mut w, &c, "b", 320);
    answer_helper(&mut w, &c, "a", 1220);
    answer_helper(&mut w, &c, "b", 1520);
    // Protected: 20 to 1520 once = 1500 s, not 2400 s. 1530 s have passed since the start; 30 s are not protected.
    assert_eq!(w.look(c.at(1530), Some("t0".into())), None);
    assert_eq!(
        w.look(c.at(2690), Some("t0".into())),
        None,
        "1190 s of the 2690 are unprotected: the window is not yet full"
    );
    assert!(w.look(c.at(2725), Some("t0".into())).is_some());
}

#[test]
fn b728_a_helper_that_repeats_the_same_call_is_ended_at_the_repetition_limit() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    open_helper(&mut w, &c, "h1", 10);
    // One helper: an input may arrive twice (the helper and the engine's own agent). The third time on is a repeat.
    for i in 0..40u64 {
        call(&mut w, &c, &format!("r{i}"), 7, 20 + i);
    }
    let b = w
        .look(c.at(100), Some("t0".into()))
        .expect("the repetition ends the run well before the grace");
    assert!(
        b.detail
            .contains("a helper agent kept repeating the same calls"),
        "{}",
        b.detail
    );
    assert_eq!(
        DirectOutcome::StoppedOther
            .label_for(Some(&b.detail))
            .split(" (")
            .next(),
        Some("Cline stopped: the harness ended the run")
    );
}

#[test]
fn b728_three_helpers_may_each_read_the_same_files() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    for (i, id) in ["a", "b", "c"].iter().enumerate() {
        open_helper(&mut w, &c, id, 10 + i as u64);
    }
    // Ten files, each read by all three helpers and once by the engine's own agent: four times each, and no repeat.
    let mut n = 0;
    for file in 0..10u64 {
        for _ in 0..4 {
            call(&mut w, &c, &format!("x{n}"), 500 + file, 20 + n);
            n += 1;
        }
    }
    assert_eq!(w.look(c.at(400), Some("t0".into())), None);
    // The fifth read of one of them is a repeat, and enough repeats end the run.
    for k in 0..40u64 {
        call(&mut w, &c, &format!("y{k}"), 500, 400 + k);
    }
    assert!(w.look(c.at(500), Some("t0".into())).is_some());
}

#[test]
fn b728_a_helper_whose_answer_never_comes_is_ended_at_the_grace_by_the_clock_alone() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    open_helper(&mut w, &c, "lost", 10);
    // No call, no answer, nothing on the wire: the module still looks at the tree, and the grace (two hours from the last change) ends the run.
    assert_eq!(w.look(c.at(7000), Some("t0".into())), None);
    assert!(
        !w.wants_look(c.at(7100), Duration::from_secs(5)),
        "nothing new, and the grace is not due"
    );
    assert!(
        w.wants_look(c.at(7200), Duration::from_secs(5)),
        "the grace is due"
    );
    let b = w.look(c.at(7200), Some("t0".into())).expect("the grace");
    assert!(
        b.detail.contains(
            "made no progress in 2 hours: a helper agent call stayed open, but no file changed"
        ),
        "{}",
        b.detail
    );
}

#[test]
fn b728_a_repeated_a_lost_or_unknown_answer_leaves_the_open_calls_right() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    open_helper(&mut w, &c, "a", 10);
    open_helper(&mut w, &c, "a", 11);
    assert_eq!(w.delegates_open(), 1, "the same id twice is one call");
    answer_helper(&mut w, &c, "nobody", 20);
    assert_eq!(
        w.delegates_open(),
        1,
        "an answer to an unknown id changes nothing"
    );
    answer_helper(&mut w, &c, "a", 30);
    answer_helper(&mut w, &c, "a", 31);
    assert_eq!(w.delegates_open(), 0);
    // A failed call and a call that arrives already ended are no wait.
    open_helper(&mut w, &c, "b", 40);
    feed_update(
        &mut w,
        c.at(41),
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "failed"}),
        tools(true),
    );
    assert_eq!(w.delegates_open(), 0);
    feed_update(
        &mut w,
        c.at(50),
        &json!({"sessionUpdate": "tool_call", "toolCallId": "z", "kind": "think", "title": "spawn_agent: x", "status": "completed"}),
        tools(true),
    );
    assert_eq!(w.delegates_open(), 0);
    // A round or a fresh session starts with no helper call open.
    open_helper(&mut w, &c, "c", 60);
    w.boundary(c.at(70));
    assert_eq!(w.delegates_open(), 0);
}

#[test]
fn b728_a_change_of_the_tree_during_a_hold_starts_the_counts_again_and_keeps_the_open_helper() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    open_helper(&mut w, &c, "h1", 10);
    assert_eq!(
        w.look(c.at(600), Some("t1".into())),
        None,
        "the tree changed"
    );
    assert_eq!(w.delegates_open(), 1);
    // The 2 hours of the grace count from that change.
    assert_eq!(w.look(c.at(7000), Some("t1".into())), None);
    assert!(w.look(c.at(600 + 7200), Some("t1".into())).is_some());
}

#[test]
fn b728_a_command_that_runs_during_a_hold_does_not_protect_the_same_time_twice() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    for i in 0..10u64 {
        w.tool_call(c.at(1 + i));
    }
    open_helper(&mut w, &c, "h1", 20);
    // A sample every 30 seconds says a command has run since before the helper began.
    for s in (50..=1220).step_by(30) {
        // the command began at second 20, with the helper: it is younger than the calls before it, so it takes none of them back
        w.command(
            c.at(s),
            Command::Active {
                age: Duration::from_secs(s - 20),
            },
        );
    }
    answer_helper(&mut w, &c, "h1", 1220);
    // Protected: 1200 s once. 1230 s have passed; if the command had added its own 1200 s the window would be open far later.
    assert_eq!(w.look(c.at(1230), Some("t0".into())), None);
    assert_eq!(w.look(c.at(2390), Some("t0".into())), None);
    assert!(
        w.look(c.at(2430), Some("t0".into())).is_some(),
        "30 + 1200 s are unprotected at 2430 s"
    );
}

#[test]
fn b728_without_a_helper_the_old_stalls_still_end_the_run() {
    let c = Clock(Instant::now());
    let mut w = watch(&c);
    for i in 0..30u64 {
        call(&mut w, &c, &format!("c{i}"), 9000, 1 + i);
        // the same input each time: nothing is credited
    }
    let b = w
        .look(c.at(60), Some("t0".into()))
        .expect("30 calls in a row");
    assert!(
        b.detail.contains("Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared."),
        "{}",
        b.detail
    );
    let mut w = watch(&c);
    for i in 0..12u64 {
        w.tool_call(c.at(1 + i * 100));
    }
    let b = w
        .look(c.at(1300), Some("t0".into()))
        .expect("20 minutes with 12 counted calls");
    assert!(b.detail.contains("Cline made no progress in 20 minutes: no file changed and 12 of 12 tool calls counted as no progress."), "{}", b.detail);
}

#[test]
fn b728_the_stop_names_what_the_harness_saw_and_the_command_to_run_next_within_the_detail_limit() {
    let c = Clock(Instant::now());
    let mut w = watch(&c).with_next_step(format!(
        "Next: ham pull {RUN} or ham continue {RUN} '<prompt>'."
    ));
    for i in 0..12u64 {
        w.tool_call(c.at(1 + i * 100));
    }
    open_helper(&mut w, &c, "h1", 1250);
    for i in 0..57u64 {
        call(&mut w, &c, &format!("k{i}"), 400 + i, 1260 + i);
    }
    answer_helper(&mut w, &c, "h1", 1500);
    let b = w.look(c.at(2800), Some("t0".into())).expect("the window");
    let label = DirectOutcome::StoppedOther.label_for(Some(&b.detail));
    assert_eq!(
        label,
        "Cline stopped: the harness ended the run (Cline made no progress in 20 minutes: no file changed and 12 of 12 tool calls counted as no progress)"
    );
    assert!(
        b.detail.contains(&format!("ham pull {RUN}")),
        "{}",
        b.detail
    );
    assert!(
        b.detail.contains(&format!("ham continue {RUN}")),
        "{}",
        b.detail
    );
    assert!(
        b.detail
            .contains("Helper agents were open 4 minutes; 57 calls not counted."),
        "{}",
        b.detail
    );
    assert!(
        b.detail.chars().count() <= harness_protocol::direct::MAX_DIRECT_DETAIL_CHARS,
        "{} chars: {}",
        b.detail.chars().count(),
        b.detail
    );
}
