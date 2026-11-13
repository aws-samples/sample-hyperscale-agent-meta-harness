//! OpenCode's wire through the watch of the goal module: the helper-agent hold and its bound read the tool calls of a
//! real `opencode acp` v2.2.6 session. The traces in `b791_traces/` are the engine's own lines (`session/update` and prompt results) of runs on
//! 2026-11-08, with the path of the scratch checkout replaced by `/work/repo`. In a trace the helper call is `name: "subagent"`,
//! `title: "subagent"`, `kind: "think"`; a shell call is titled `shell` while pending and then by its command text.
#![allow(clippy::unwrap_used)]

use agentd::direct::goal::watch::{
    HelperLimits, HelperRule, Watch, WatchLimits, WireTools, feed_update,
};
use agentd::engine::goal::goal_engine;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn trace(name: &str) -> Vec<(f64, Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/b791_traces")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (v["t"].as_f64().unwrap(), v["msg"].clone())
        })
        .collect()
}

fn update(msg: &Value) -> Option<&Value> {
    (msg["method"] == "session/update").then(|| &msg["params"]["update"])
}

fn tools() -> WireTools<'static> {
    let g = goal_engine("opencode").unwrap();
    WireTools {
        completion: g.completion_tool,
        delegate: g.delegate_tool,
        title: g.title,
    }
}

fn watch(t0: Instant, silence: u64, wait: u64) -> Watch {
    Watch::new(
        WatchLimits {
            calls: 30,
            window: Duration::from_secs(20 * 60),
            min_calls: 10,
            who: "OpenCode",
        },
        t0,
        Some("t0".into()),
    )
    .with_helper_limits(HelperLimits {
        silence: Duration::from_secs(silence),
        wait: Duration::from_secs(wait),
    })
}

/// Feeds the trace's updates to the watch, at the time the engine sent them, up to (not including) the first line for which `stop` is true.
fn feed(w: &mut Watch, t0: Instant, lines: &[(f64, Value)], stop: impl Fn(&Value) -> bool) {
    for (t, msg) in lines {
        let Some(u) = update(msg) else { continue };
        if stop(u) {
            return;
        }
        feed_update(w, t0 + Duration::from_secs_f64(*t), u, tools());
    }
}

#[test]
fn b791_the_traces_are_the_engines_own_lines() {
    for name in [
        "opencode-2.2.6-subagent-answers.jsonl",
        "opencode-2.2.6-subagent-hangs-then-cancel.jsonl",
        "opencode-2.2.6-goal-attached-shell-call.jsonl",
        "opencode-2.2.6-plain-turn-tools.jsonl",
    ] {
        let lines = trace(name);
        assert!(lines.len() >= 5, "{name}");
        assert!(
            lines
                .iter()
                .any(|(_, m)| m["result"]["stopReason"].is_string()),
            "{name} ends with a prompt result"
        );
        let all = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/b791_traces")
                .join(name),
        )
        .unwrap();
        assert!(
            !all.contains(&["", "local", "home"].join("/")),
            "{name} names a host path"
        );
    }
}

#[test]
fn b791_a_recorded_subagent_call_opens_the_helper_hold_and_its_answer_closes_it() {
    let t0 = Instant::now();
    let lines = trace("opencode-2.2.6-subagent-answers.jsonl");
    let mut w = watch(t0, 1500, 4500);
    // up to the call's completion the hold is open
    feed(&mut w, t0, &lines, |u| {
        u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"
    });
    assert_eq!(
        w.delegates_open(),
        1,
        "the pending call and its in_progress updates are one helper call"
    );
    assert!(w.oldest_helper(t0 + Duration::from_secs(10)).is_some());
    // then the whole trace: the completed update closes it
    let mut w = watch(t0, 1500, 4500);
    feed(&mut w, t0, &lines, |_| false);
    assert_eq!(w.delegates_open(), 0);
    assert_eq!(w.oldest_helper(t0 + Duration::from_secs(60)), None);
}

#[test]
fn b791_a_recorded_subagent_call_that_never_answers_stalls_at_the_silence_limit() {
    let t0 = Instant::now();
    let lines = trace("opencode-2.2.6-subagent-hangs-then-cancel.jsonl");
    let mut w = watch(t0, 1500, 4500);
    // everything the engine said before the cancel ended the call
    feed(&mut w, t0, &lines, |u| {
        u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed"
    });
    assert_eq!(w.delegates_open(), 1);
    assert!(
        w.helper_stall(t0 + Duration::from_secs(1400)).is_none(),
        "inside the limit"
    );
    let stall = w
        .helper_stall(t0 + Duration::from_secs(1520))
        .expect("the silence limit of 25 minutes passed with the helper call open");
    assert_eq!(stall.rule, HelperRule::Silence);
    assert_eq!(stall.open, 1);
}

#[test]
fn b791_a_recorded_cancel_closes_the_hold_through_the_failed_update() {
    let t0 = Instant::now();
    let lines = trace("opencode-2.2.6-subagent-hangs-then-cancel.jsonl");
    let mut w = watch(t0, 1500, 4500);
    feed(&mut w, t0, &lines, |_| false);
    assert_eq!(
        w.delegates_open(),
        0,
        "a failed update ends the helper call"
    );
    assert!(w.helper_stall(t0 + Duration::from_secs(5000)).is_none());
}

#[test]
fn b791_shell_and_write_calls_are_counted_and_never_held() {
    let t0 = Instant::now();
    let lines = trace("opencode-2.2.6-plain-turn-tools.jsonl");
    let mut w = watch(t0, 1500, 4500);
    // up to the model's first words after the calls (new text starts the count again)
    feed(&mut w, t0, &lines, |u| {
        u["sessionUpdate"] == "agent_message_chunk"
    });
    assert_eq!(w.delegates_open(), 0, "none of them is a helper agent");
    assert!(
        w.calls_in_a_row() >= 2,
        "the calls were counted: {}",
        w.calls_in_a_row()
    );
}

#[test]
fn b791_the_title_of_a_shell_call_changes_to_its_command_and_still_is_one_call() {
    let t0 = Instant::now();
    let lines = trace("opencode-2.2.6-goal-attached-shell-call.jsonl");
    let pending = lines
        .iter()
        .filter_map(|(_, m)| update(m))
        .find(|u| u["sessionUpdate"] == "tool_call" && u["name"] == "shell")
        .expect("a pending shell call");
    assert_eq!(pending["title"], "shell");
    let running = lines
        .iter()
        .filter_map(|(_, m)| update(m))
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "in_progress")
        .expect("a running update");
    assert_eq!(
        running["title"], "cat hello.txt",
        "the command text is the title from then on"
    );
    let mut w = watch(t0, 1500, 4500);
    // the calls only: text of the model starts the count again
    for (t, msg) in &lines {
        if let Some(u) = update(msg).filter(|u| u["sessionUpdate"] != "agent_message_chunk") {
            feed_update(&mut w, t0 + Duration::from_secs_f64(*t), u, tools());
        }
    }
    assert_eq!(w.calls_in_a_row(), 1);
    assert_eq!(w.delegates_open(), 0);
}

#[test]
fn b791_a_call_titled_like_the_helper_tool_with_a_longer_name_is_another_tool() {
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500);
    for (i, title) in ["subagents", "subagent2", "my-subagent"].iter().enumerate() {
        let u = serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": format!("c{i}"), "title": title, "status": "pending", "kind": "other"});
        feed_update(&mut w, t0, &u, tools());
    }
    assert_eq!(w.delegates_open(), 0);
}

#[test]
fn b791_no_title_counts_as_a_completion_call_when_the_engine_has_no_completion_tool() {
    // the empty name is a prefix of every title; it must never be read as "this call completes the goal"
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500);
    for i in 0..3 {
        let u = serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": format!("c{i}"), "title": "grep", "status": "pending", "kind": "search"});
        feed_update(&mut w, t0, &u, tools());
    }
    assert_eq!(w.calls_in_a_row(), 3);
}

// ---- the credit for calls that bring something new -------------------------------------------

/// A shell call as OpenCode puts it on the wire: pending with an empty `rawInput`, the input and the command title with the first running
/// update, the answer in `content`.
fn shell_call(w: &mut Watch, t0: Instant, n: u32, command: &str, answer: &str) {
    let id = format!("call-{n}");
    let at = t0 + Duration::from_secs(u64::from(n));
    let mut ev = |u: Value| feed_update(w, at, &u, tools());
    ev(
        serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": id, "name": "shell", "title": "shell", "kind": "execute", "status": "pending", "rawInput": {}}),
    );
    ev(
        serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "kind": "execute", "title": command, "status": "in_progress",
        "rawInput": {"command": command, "cwd": "/work/repo"}}),
    );
    ev(
        serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": answer}}], "rawOutput": {}}),
    );
}

#[test]
fn b791_calls_with_new_inputs_and_new_answers_are_given_back_as_they_are_for_cline() {
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    for n in 0..40 {
        shell_call(
            &mut w,
            t0,
            n,
            &format!("grep -rn thing{n} src"),
            &format!("src/a{n}.rs:1:x\nsrc/b{n}.rs:2:y\nsrc/c{n}.rs:3:z"),
        );
    }
    assert_eq!(
        w.new_credits(),
        40,
        "every call had a new input and three new lines"
    );
    assert!(
        w.calls_in_a_row() < 30,
        "exploring a large tree is no stall: {}",
        w.calls_in_a_row()
    );
}

#[test]
fn b791_the_same_call_over_and_over_earns_nothing() {
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    for n in 0..40 {
        shell_call(&mut w, t0, n, "cat notes.txt", "one\ntwo\nthree");
    }
    // the first call earned its credit (new input, new lines); the other 39 are the same input with the same answer
    assert_eq!(w.new_credits(), 1);
    assert_eq!(w.calls_in_a_row(), 39);
}

#[test]
fn b791_a_failed_call_or_a_short_answer_earns_nothing() {
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    shell_call(&mut w, t0, 1, "ls a", "only\ntwo");
    let failed = serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "failed",
        "content": [{"type": "content", "content": {"type": "text", "text": "x\ny\nz"}}], "rawOutput": {"error": "boom"}});
    feed_update(&mut w, t0, &failed, tools());
    assert_eq!(w.new_credits(), 0);
    assert_eq!(w.calls_in_a_row(), 1);
}

#[test]
fn b791_clines_wire_is_read_as_it_was() {
    // a Cline-style call: the input and the title come with the tool_call, the answer is in rawOutput; the content blocks are not read for it
    let t0 = Instant::now();
    let cline = WireTools {
        completion: "mark_goal_complete",
        delegate: Some("spawn_agent"),
        title: goal_engine("cline").unwrap().title,
    };
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    let call = serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "run_commands: ls", "kind": "execute", "status": "pending",
        "rawInput": {"commands": ["ls"]}});
    feed_update(&mut w, t0, &call, cline);
    let done = serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": "a\nb\nc\nd"}}], "rawOutput": [{"query": "ls", "result": "a\nb", "success": true}]});
    feed_update(&mut w, t0, &done, cline);
    assert_eq!(
        w.new_credits(),
        0,
        "two lines in rawOutput are under the minimum; the content blocks of a Cline update do not count"
    );
}

#[test]
fn b791_metadata_that_changes_does_not_make_an_answer_new() {
    // The same text from two calls, with different strings in rawOutput: the answer is the text of the blocks, so the second call is not new.
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    for n in 0..2u32 {
        let id = format!("call-{n}");
        let mut ev = |u: Value| feed_update(&mut w, t0, &u, tools());
        ev(
            serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": id, "name": "shell", "title": "shell", "kind": "execute", "status": "pending", "rawInput": {}}),
        );
        ev(
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "kind": "execute", "title": format!("cat file{n}"), "status": "in_progress",
            "rawInput": {"command": format!("cat file{n}")}}),
        );
        ev(
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "one\ntwo\nthree"}}],
            "rawOutput": {"metadata": {"title": format!("run {n}"), "output": format!("output {n}")}}}),
        );
    }
    assert_eq!(
        w.new_credits(),
        1,
        "the first call earned its credit, the second brought the same three lines"
    );
}

#[test]
fn b791_a_block_that_is_not_text_is_not_an_answer() {
    let t0 = Instant::now();
    let mut w = watch(t0, 1500, 4500).with_new_calls(100);
    let mut ev = |u: Value| feed_update(&mut w, t0, &u, tools());
    ev(
        serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "c", "name": "read", "title": "read", "kind": "read", "status": "pending", "rawInput": {}}),
    );
    ev(
        serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "c", "kind": "read", "title": "read", "status": "in_progress", "rawInput": {"path": "a.png"}}),
    );
    ev(
        serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "c", "status": "completed",
        "content": [{"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": "a\nb\nc\nd"}}], "rawOutput": {}}),
    );
    assert_eq!(w.new_credits(), 0);
}
