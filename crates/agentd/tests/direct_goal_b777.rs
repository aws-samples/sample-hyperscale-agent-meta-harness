//! The wait for helper agents is bounded.
//!
//! Two release-gate runs were ended by the two hour guard although their code was done: the engine's helper agents (reviewers that start further
//! reviewers) were still working, the engine waited for them, and no file could change. The stream was never quiet, so the silence limit did not
//! apply, and the hold on the brake paused the clock for the whole time. Their events carry no name of the agent that made them, so what
//! can be told apart is when a helper call was opened and when one was answered. This file replays those two runs and five runs whose helpers
//! returned, through the same code the module feeds the wire through, and tests the limits with a clock the tests move.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::watch::{
    HelperLimits, HelperRule, Watch, WatchLimits, WireTools, feed_update,
};
use agentd::engine::tools::TitleStyle;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct Clock(Instant);

impl Clock {
    fn at(&self, s: u64) -> Instant {
        self.0 + Duration::from_secs(s)
    }
}

fn limits() -> WatchLimits {
    WatchLimits {
        calls: 30,
        window: Duration::from_secs(20 * 60),
        min_calls: 10,
        who: "Cline",
    }
}

const SILENCE: u64 = 25 * 60;
const WAIT: u64 = 75 * 60;

fn helper_limits(silence: u64, wait: u64) -> HelperLimits {
    HelperLimits {
        silence: Duration::from_secs(silence),
        wait: Duration::from_secs(wait),
    }
}

fn watch(c: &Clock, silence: u64, wait: u64) -> Watch {
    Watch::new(limits(), c.at(0), Some("t0".into()))
        .with_helper_limits(helper_limits(silence, wait))
}

fn tools() -> WireTools<'static> {
    WireTools {
        completion: "mark_goal_complete",
        delegate: Some("spawn_agent"),
        title: TitleStyle::NameColonArgs,
    }
}

fn open(w: &mut Watch, c: &Clock, id: &str, s: u64) {
    feed_update(
        w,
        c.at(s),
        &json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "think", "title": "spawn_agent: review", "status": "pending"}),
        tools(),
    );
}

fn answer(w: &mut Watch, c: &Clock, id: &str, s: u64) {
    feed_update(
        w,
        c.at(s),
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
            "rawOutput": {"finishReason": "completed", "iterations": 9, "text": "findings"}}),
        tools(),
    );
}

fn aborted(w: &mut Watch, c: &Clock, id: &str, s: u64) {
    feed_update(
        w,
        c.at(s),
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
            "rawOutput": {"finishReason": "aborted", "iterations": 9, "text": ""}}),
        tools(),
    );
}

#[test]
fn b777_the_tracker_knows_each_open_helper_call_and_how_long_it_has_been_open() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, WAIT);
    assert_eq!(w.delegates_open(), 0);
    assert_eq!(w.oldest_helper(c.at(5)), None);
    open(&mut w, &c, "a", 10);
    open(&mut w, &c, "b", 100);
    open(&mut w, &c, "a", 200);
    assert_eq!(w.delegates_open(), 2, "the same id twice is one call");
    assert_eq!(
        w.oldest_helper(c.at(300)),
        Some(Duration::from_secs(290)),
        "the age of the oldest call, from its first announcement"
    );
    answer(&mut w, &c, "a", 310);
    assert_eq!(w.delegates_open(), 1);
    assert_eq!(w.oldest_helper(c.at(400)), Some(Duration::from_secs(300)));
    answer(&mut w, &c, "b", 410);
    assert_eq!(w.delegates_open(), 0);
    assert_eq!(w.oldest_helper(c.at(500)), None);
}

#[test]
fn b777_the_silence_clock_restarts_when_a_helper_call_is_answered() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, WAIT);
    open(&mut w, &c, "a", 10);
    open(&mut w, &c, "b", 20);
    // 24 minutes after the hold began nothing is wrong yet
    assert!(w.helper_stall(c.at(24 * 60)).is_none());
    answer(&mut w, &c, "a", 24 * 60 + 10);
    // the clock began again at the answer: 24 minutes later is still inside the limit
    assert!(w.helper_stall(c.at(48 * 60)).is_none());
    // and 25 minutes after the answer it is not
    let s = w
        .helper_stall(c.at(24 * 60 + 10 + SILENCE))
        .expect("silent for the limit");
    assert_eq!(s.rule, HelperRule::Silence);
    assert_eq!(s.open, 1);
    assert_eq!(s.silent, Duration::from_secs(SILENCE));
}

#[test]
fn b777_a_helper_call_that_begins_does_not_restart_the_silence_clock() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, WAIT);
    open(&mut w, &c, "root", 0);
    // reviewers that start further reviewers keep the stream busy and the tree growing; only an answer is progress
    for (k, s) in [600u64, 1200, 1400].iter().enumerate() {
        open(&mut w, &c, &format!("child{k}"), *s);
    }
    assert!(w.helper_stall(c.at(SILENCE - 1)).is_none());
    let s = w
        .helper_stall(c.at(SILENCE))
        .expect("the begun calls are no answer");
    assert_eq!(s.rule, HelperRule::Silence);
    assert_eq!(s.open, 4);
}

#[test]
fn b777_a_stall_is_raised_once_after_the_limit_and_not_again_until_the_watch_is_reset() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 600, WAIT);
    open(&mut w, &c, "a", 0);
    assert!(w.helper_stall(c.at(599)).is_none());
    assert!(w.helper_stall(c.at(600)).is_some());
    assert!(
        w.helper_stall(c.at(601)).is_none(),
        "the cancel is on its way: raised once"
    );
    assert!(w.helper_stall(c.at(5000)).is_none());
    w.helper_reset(c.at(5010));
    assert_eq!(w.delegates_open(), 0, "the cancelled calls are gone");
    assert!(
        w.helper_stall(c.at(9000)).is_none(),
        "no helper call is open"
    );
    // a new wait gets a new clock
    open(&mut w, &c, "b", 9100);
    assert!(w.helper_stall(c.at(9100 + 599)).is_none());
    assert!(w.helper_stall(c.at(9100 + 600)).is_some());
}

#[test]
fn b777_no_stall_while_answers_keep_arriving_inside_the_limit() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, 10 * 60 * 60);
    open(&mut w, &c, "root", 0);
    // a helper tree that answers every 20 minutes for three hours is slow, not stalled
    for k in 1..=9u64 {
        let t = k * 20 * 60;
        open(&mut w, &c, &format!("leaf{k}"), t - 60);
        assert!(w.helper_stall(c.at(t - 1)).is_none(), "at {t}");
        answer(&mut w, &c, &format!("leaf{k}"), t);
    }
    assert_eq!(w.delegates_open(), 1);
}

#[test]
fn b777_the_oldest_open_call_stalls_the_wait_even_when_other_helpers_keep_answering() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, WAIT);
    open(&mut w, &c, "stuck", 0);
    for k in 1..=4u64 {
        let t = k * 15 * 60;
        open(&mut w, &c, &format!("leaf{k}"), t - 30);
        answer(&mut w, &c, &format!("leaf{k}"), t);
        assert!(
            w.helper_stall(c.at(t)).is_none(),
            "an answer a quarter hour ago, at {t}"
        );
    }
    let s = w
        .helper_stall(c.at(WAIT))
        .expect("75 minutes since the oldest call opened");
    assert_eq!(s.rule, HelperRule::Wait);
    assert_eq!(s.oldest, Duration::from_secs(WAIT));
    assert_eq!(s.open, 1);
}

#[test]
fn b777_a_limit_of_zero_switches_that_rule_off() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 0, WAIT);
    open(&mut w, &c, "a", 0);
    assert!(w.helper_stall(c.at(WAIT - 1)).is_none(), "no silence rule");
    assert_eq!(w.helper_stall(c.at(WAIT)).unwrap().rule, HelperRule::Wait);
    let mut w = watch(&c, SILENCE, 0);
    open(&mut w, &c, "a", 0);
    assert_eq!(
        w.helper_stall(c.at(SILENCE)).unwrap().rule,
        HelperRule::Silence
    );
    let mut w = watch(&c, 0, 0);
    open(&mut w, &c, "a", 0);
    assert!(
        w.helper_stall(c.at(100 * 3600)).is_none(),
        "both off: the two hour guard is the only limit"
    );
    // the default of a watch that was not given limits is off, as it was before
    let mut w = Watch::new(limits(), c.at(0), Some("t0".into()));
    open(&mut w, &c, "a", 0);
    assert!(w.helper_stall(c.at(100 * 3600)).is_none());
}

#[test]
fn b777_the_calls_a_cancel_aborts_are_no_answers_and_a_reset_leaves_a_clean_watch() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 600, WAIT);
    open(&mut w, &c, "a", 0);
    open(&mut w, &c, "b", 5);
    assert!(w.helper_stall(c.at(600)).is_some());
    // the engine reports each aborted helper call right after the cancel
    aborted(&mut w, &c, "a", 601);
    aborted(&mut w, &c, "b", 601);
    assert_eq!(w.delegates_open(), 0);
    w.helper_reset(c.at(602));
    // a helper call the new prompt starts has its own clock, not the old one
    open(&mut w, &c, "c", 700);
    assert!(w.helper_stall(c.at(700 + 599)).is_none());
    assert!(w.helper_stall(c.at(700 + 600)).is_some());
    // an aborted call outside a stall closes the call but is no progress either
    let mut w = watch(&c, 600, WAIT);
    open(&mut w, &c, "x", 0);
    open(&mut w, &c, "y", 100);
    aborted(&mut w, &c, "x", 500);
    assert_eq!(w.delegates_open(), 1);
    assert!(
        w.helper_stall(c.at(600)).is_some(),
        "the clock still runs from the hold's start"
    );
}

#[test]
fn b777_a_boundary_clears_the_helper_calls_and_the_stall() {
    let c = Clock(Instant::now());
    let mut w = watch(&c, 600, WAIT);
    open(&mut w, &c, "a", 0);
    assert!(w.helper_stall(c.at(600)).is_some());
    w.boundary(c.at(610));
    assert_eq!(w.delegates_open(), 0);
    open(&mut w, &c, "b", 620);
    assert!(
        w.helper_stall(c.at(620 + 600)).is_some(),
        "a fresh session can stall again"
    );
}

// ---- the recorded runs ------------------------------------------------------------------------------------------------------------

const STALLED: [(&str, &str); 2] = [
    ("events", include_str!("b777_traces/stalled-events.json")),
    (
        "enterprise",
        include_str!("b777_traces/stalled-enterprise.json"),
    ),
];
const FINISHED: [(&str, &str); 5] = [
    ("1", include_str!("b777_traces/finished-1.json")),
    ("2", include_str!("b777_traces/finished-2.json")),
    ("3", include_str!("b777_traces/finished-3.json")),
    ("4", include_str!("b777_traces/finished-4.json")),
    ("5", include_str!("b777_traces/finished-5.json")),
];

/// Replays the helper calls of a recorded run with the defaults, looking every 30 seconds as the module does. Returns when the first stall
/// came, and its rule, if one came before the guard's cancel or the end of the recording.
fn replay(trace: &str) -> Option<(u64, HelperRule)> {
    let v: Value = serde_json::from_str(trace).unwrap();
    let calls = v["calls"].as_array().unwrap();
    let c = Clock(Instant::now());
    let mut w = watch(&c, SILENCE, WAIT);
    // (second, order, open or answer, id): an answer comes before an open of the same second
    let mut evs: Vec<(u64, u8, bool, String)> = Vec::new();
    for call in calls {
        let id = call["id"].as_str().unwrap().to_string();
        evs.push((call["open_s"].as_u64().unwrap(), 1, true, id.clone()));
        if let Some(a) = call["answer_s"].as_u64() {
            evs.push((a, 0, false, id));
        }
    }
    evs.sort();
    let last = v["cancelAtS"]
        .as_u64()
        .unwrap_or_else(|| evs.last().unwrap().0);
    let mut next = 0usize;
    let mut t = 0u64;
    while t <= last {
        while next < evs.len() && evs[next].0 <= t {
            let (s, _, is_open, id) = &evs[next];
            if *is_open {
                open(&mut w, &c, id, *s);
            } else {
                answer(&mut w, &c, id, *s);
            }
            next += 1;
        }
        if let Some(s) = w.helper_stall(c.at(t)) {
            return Some((t, s.rule));
        }
        t += 30;
    }
    None
}

#[test]
fn b777_both_recorded_runs_that_the_two_hour_guard_ended_are_stalled_long_before_it() {
    for (name, trace) in STALLED {
        let v: Value = serde_json::from_str(trace).unwrap();
        let cancel = v["cancelAtS"].as_u64().unwrap();
        assert!(
            cancel >= 7000,
            "{name}: TRIGGER PRESENT, the guard cancelled at {cancel} s"
        );
        let (at, rule) = replay(trace).unwrap_or_else(|| panic!("{name}: no stall"));
        assert!(
            at + 40 * 60 <= cancel,
            "{name}: the stall at {at} s is at least 40 minutes before the guard at {cancel} s"
        );
        assert!(
            at > 20 * 60,
            "{name}: and not in the first 20 minutes: {at} s"
        );
        eprintln!("{name}: stall at {at} s by the {rule:?} rule, guard at {cancel} s");
    }
}

#[test]
fn b777_none_of_the_five_recorded_runs_whose_helpers_returned_is_stalled() {
    for (name, trace) in FINISHED {
        assert_eq!(
            replay(trace),
            None,
            "run {name}: its helpers all returned, the default limits must let it be"
        );
    }
}

// ---- the stage variables -------------------------------------------------------------------------------------------------------------

fn settings_of(pairs: &[(&str, &str)]) -> agentd::direct::goal::settings::GoalSettings {
    let m: std::collections::BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    agentd::direct::goal::settings::GoalSettings::from_lookup(&|k| m.get(k).cloned())
}

#[test]
fn b777_the_limits_have_opinionated_defaults_and_a_zero_that_switches_each_off() {
    let s = settings_of(&[]);
    assert_eq!(s.helper_silence, Duration::from_secs(25 * 60));
    assert_eq!(s.helper_wait, Duration::from_secs(75 * 60));
    assert_eq!(s.helper_limits(), helper_limits(SILENCE, WAIT));
    let s = settings_of(&[
        ("AGENTD_GOAL_HELPER_SILENCE_S", "0"),
        ("AGENTD_GOAL_HELPER_WAIT_S", "600"),
    ]);
    assert_eq!(s.helper_limits(), helper_limits(0, 600));
    let s = settings_of(&[
        ("AGENTD_GOAL_HELPER_SILENCE_S", " 90 "),
        ("AGENTD_GOAL_HELPER_WAIT_S", "0"),
    ]);
    assert_eq!(
        s.helper_limits(),
        helper_limits(90, 0),
        "blanks around the number do not matter"
    );
}

#[test]
fn b777_a_value_out_of_range_is_cut_to_its_bound_and_one_that_is_no_number_is_the_default() {
    // the module's rule for every stage variable: bounds cut, nonsense is the default (a request member would be refused instead)
    let s = settings_of(&[
        ("AGENTD_GOAL_HELPER_SILENCE_S", "99999999"),
        ("AGENTD_GOAL_HELPER_WAIT_S", "1000000"),
    ]);
    assert_eq!(s.helper_limits(), helper_limits(86_400, 86_400));
    for bad in ["abc", "-5", "1.5", "", "20m"] {
        let s = settings_of(&[
            ("AGENTD_GOAL_HELPER_SILENCE_S", bad),
            ("AGENTD_GOAL_HELPER_WAIT_S", bad),
        ]);
        assert_eq!(s.helper_limits(), helper_limits(SILENCE, WAIT), "{bad:?}");
    }
}
