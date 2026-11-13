//! The recorder of a direct run: what agentd keeps of the tool calls and messages
//! of Cline so that a stop can be explained afterwards, bounded and redacted at write time. Until now the collector threw away the
//! `rawInput` and `rawOutput` Cline sends over ACP and kept a cut title and a status; a command that exited 1 or hit the 30 s shell
//! cut read "completed". These tests pin the record (ring, heads and tails, verdicts), the trusted anomaly lines, the redaction, the
//! counters, the accessors the needs-input card and the fresh-session digest read, and the digest written at every stop (at most
//! 64 KiB). Pure: no engine, no clock, no network. Test stand-ins for key IDs and `name=value` secrets are built at run time.
#![allow(clippy::unwrap_used)]

use agentd::acp::AcpHandler;
use agentd::direct::collector::DirectCollector;
use agentd::direct::engine_stop::AttemptFacts;
use agentd::direct::recorder::{
    COMPACT_FIELD_BYTES, CallVerdict, ERROR_TEXT_BYTES, INPUT_HEAD_BYTES, LAST_ERROR_CHARS,
    MESSAGE_BYTES, MESSAGES_KEPT, NOTES_LISTED, OUTPUT_TAIL_BYTES, RING_CALLS, Recorder,
    STOP_DIGEST_MAX_BYTES, StopContext, TEST_TAIL_BYTES, TITLE_CHARS,
};
use agentd::events::EventRecorder;
use harness_protocol::envelope::{EventType, Payload};
use serde_json::{Value, json};

// ---------------------------------------------------------------- helpers

fn call(id: &str, title: &str, kind: &str, input: Value) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind,
           "status": "pending", "rawInput": input})
}

fn done(id: &str, status: &str, output: Value) -> Value {
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "rawOutput": output})
}

/// What Cline's shell tool returns: a JSON array with one item per command, as the text of the result.
fn shell_result(cmd: &str, out: &str, success: bool) -> Value {
    Value::String(json!([{"query": cmd, "result": out, "success": success}]).to_string())
}

fn run_cmd(r: &mut Recorder, id: &str, cmd: &str, out: &str) {
    r.on_tool_call(&call(
        id,
        &format!("run_commands: {cmd}"),
        "execute",
        json!({"commands": [cmd]}),
    ));
    r.on_tool_call_update(&done(id, "completed", shell_result(cmd, out, true)));
}

fn only_call(r: &Recorder) -> agentd::direct::recorder::ToolRecord {
    let calls = r.calls();
    assert_eq!(calls.len(), 1, "one call in the ring: {calls:?}");
    calls[0].clone()
}

fn facts(tool_calls: u64, completed: u64, failed_tail: u32, identical_tail: u32) -> AttemptFacts {
    AttemptFacts {
        tool_calls,
        completed,
        failed_tail,
        identical_tail,
    }
}

fn ctx() -> StopContext {
    StopContext {
        outcome: Some("stopped_engine_cancelled".into()),
        label: Some("Cline stopped: Cline ended the run itself".into()),
        detail: Some("the engine ended the prompt cancelled with no cancel request".into()),
        stop_reason: Some("cancelled".into()),
        session: 1,
        run_s: 412,
        files_changed: Some(3),
        model: Some("us.anthropic.claude-opus-5-5".into()),
        final_message: None,
    }
}

/// A key ID built at run time, so no key-shaped literal sits in the repository.
fn key_id() -> String {
    format!("{}{}", "ASIA", "TESTKEYTESTKEY12")
}

/// A `name=value` secret built at run time.
fn named_secret() -> (String, String) {
    let value = ["hunter2", "hunter2"].concat();
    (format!("DB_{}={}", ["PASS", "WORD"].concat(), value), value)
}

fn update(u: Value) -> Value {
    json!({"sessionId": "s1", "update": u})
}

fn chunk(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

// ---------------------------------------------------------------- the record of a call

#[test]
fn a_tool_call_opens_a_record_with_title_kind_keys_and_the_head_of_its_input() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: cargo test",
        "execute",
        json!({"commands": ["cargo test"]}),
    ));
    let c = only_call(&r);
    assert_eq!(c.n, 1);
    assert_eq!(c.attempt, 1);
    assert_eq!(c.id, "t1");
    assert_eq!(c.tool, "run_commands");
    assert_eq!(c.kind, "execute");
    assert_eq!(c.title, "run_commands: cargo test");
    assert_eq!(c.engine_status, "pending");
    assert_eq!(c.input_keys, vec!["commands".to_string()]);
    assert!(c.input_head.contains("cargo test"), "{}", c.input_head);
    assert_eq!(
        c.verdict,
        CallVerdict::NoResult,
        "no terminal update has arrived"
    );
}

#[test]
fn a_long_title_is_cut_to_the_title_limit() {
    let mut r = Recorder::new(vec![]);
    let title = format!("run_commands: {}", "x".repeat(400));
    r.on_tool_call(&call("t1", &title, "execute", json!({"commands": ["x"]})));
    assert_eq!(only_call(&r).title.chars().count(), TITLE_CHARS);
}

#[test]
fn the_input_head_is_cut_at_the_limit_on_a_character_boundary_and_the_full_size_is_kept() {
    let mut r = Recorder::new(vec![]);
    let big = "\u{1D11E}".repeat(5_000); // 4 bytes each
    r.on_tool_call(&call(
        "t1",
        "editor: big",
        "edit",
        json!({"path": "a.txt", "new_text": big}),
    ));
    let c = only_call(&r);
    assert!(
        c.input_head.len() <= INPUT_HEAD_BYTES,
        "{} bytes",
        c.input_head.len()
    );
    assert!(
        c.input_head.len() > INPUT_HEAD_BYTES - 8,
        "the head is as long as the limit allows"
    );
    assert!(
        c.input_bytes > 8_000,
        "full size recorded: {}",
        c.input_bytes
    );
}

#[test]
fn a_result_with_no_error_reads_completed() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "ls", "a\nb\n");
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Ok);
    assert_eq!(c.engine_status, "completed");
    assert!(c.output_tail.contains("a\nb"), "{}", c.output_tail);
    assert_eq!(c.error, None);
    assert_eq!(c.exit_code, None);
}

#[test]
fn a_nonzero_exit_the_engine_calls_completed_reads_failed() {
    let mut r = Recorder::new(vec![]);
    run_cmd(
        &mut r,
        "t1",
        "pytest -q",
        "[Command exited with code 1]\nFAILED tests/test_a.py::test_a - assert 1 == 2\n",
    );
    let c = only_call(&r);
    assert_eq!(
        c.engine_status, "completed",
        "the engine's own word is kept as it was"
    );
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert_eq!(c.exit_code, Some(1));
    assert_eq!(r.tool_notes()[0].status, "failed");
}

#[test]
fn an_exit_code_of_zero_is_not_a_failure() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "true", "[Command exited with code 0]\nok\n");
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Ok);
    assert_eq!(c.exit_code, Some(0));
}

#[test]
fn the_30_second_shell_cut_reads_timed_out() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "sleep 60", "Command timed out after 30000ms");
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::TimedOut);
    assert_eq!(r.tool_notes()[0].status, "timed_out");
}

#[test]
fn success_false_in_a_result_item_reads_failed_with_its_error_text() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: make",
        "execute",
        json!({"commands": ["make"]}),
    ));
    r.on_tool_call_update(&done(
        "t1",
        "completed",
        Value::String(
            json!([{"query": "make", "result": "", "success": false, "error": "make: *** No rule to make target"}])
                .to_string(),
        ),
    ));
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert!(c.error.unwrap().contains("No rule to make target"));
}

#[test]
fn an_engine_failed_status_reads_failed_and_keeps_a_plain_error_text() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "read_files: nope.txt",
        "read",
        json!({"files": [{"path": "nope.txt"}]}),
    ));
    r.on_tool_call_update(&done("t1", "failed", json!("ENOENT: no such file")));
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert_eq!(c.engine_status, "failed");
    assert!(c.error.unwrap().contains("ENOENT"));
}

#[test]
fn a_structured_raw_output_is_read_like_its_text() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: false",
        "execute",
        json!({"commands": ["false"]}),
    ));
    r.on_tool_call_update(&done(
        "t1",
        "completed",
        json!([{"query": "false", "result": "[Command exited with code 1]", "success": true}]),
    ));
    assert_eq!(only_call(&r).verdict, CallVerdict::Failed);
}

#[test]
fn a_call_with_no_terminal_update_is_listed_as_no_result() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: sleep 600",
        "execute",
        json!({"commands": ["sleep 600"]}),
    ));
    assert_eq!(r.tool_notes()[0].status, "no_result");
}

#[test]
fn an_update_for_a_call_that_left_the_ring_is_ignored() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call_update(&done("never-seen", "failed", json!("x")));
    assert!(r.calls().is_empty());
}

#[test]
fn the_output_tail_keeps_the_end_and_is_bounded() {
    let mut r = Recorder::new(vec![]);
    let out = format!("{}\nTHE-LAST-LINE", "line of output\n".repeat(2_000));
    run_cmd(&mut r, "t1", "build", &out);
    let c = only_call(&r);
    assert!(
        c.output_tail.len() <= OUTPUT_TAIL_BYTES,
        "{} bytes",
        c.output_tail.len()
    );
    assert!(c.output_tail.ends_with("THE-LAST-LINE") || c.output_tail.contains("THE-LAST-LINE"));
    assert!(c.output_bytes > 20_000, "full size recorded");
}

#[test]
fn a_long_error_text_is_bounded() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call("t1", "read_files: x", "read", json!({"files": []})));
    r.on_tool_call_update(&done("t1", "failed", json!("e".repeat(5_000))));
    assert!(only_call(&r).error.unwrap().len() <= ERROR_TEXT_BYTES);
}

#[test]
fn the_ring_keeps_the_newest_calls_and_numbers_them_through_the_run() {
    let mut r = Recorder::new(vec![]);
    for i in 1..=45 {
        run_cmd(&mut r, &format!("t{i}"), &format!("echo {i}"), "ok");
    }
    let calls = r.calls();
    assert_eq!(calls.len(), RING_CALLS);
    assert_eq!(calls.first().unwrap().n, 6);
    assert_eq!(calls.last().unwrap().n, 45);
    assert_eq!(r.counters().calls, 45);
    let notes = r.tool_notes();
    assert_eq!(notes.len(), NOTES_LISTED);
    assert_eq!(notes.last().unwrap().title, "run_commands: echo 45");
    assert_eq!(notes.first().unwrap().title, "run_commands: echo 16");
}

#[test]
fn begin_prompt_numbers_the_attempts_and_keeps_the_ring() {
    // The driver calls `begin_prompt` before every `session/prompt`, so the first prompt is attempt 1.
    let mut r = Recorder::new(vec![]);
    r.begin_prompt();
    run_cmd(&mut r, "t1", "echo 1", "ok");
    r.begin_prompt();
    run_cmd(&mut r, "t2", "echo 2", "ok");
    let calls = r.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].attempt, 1);
    assert_eq!(calls[1].attempt, 2);
}

#[test]
fn calls_recorded_before_any_prompt_began_belong_to_attempt_one() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "echo 1", "ok");
    assert_eq!(only_call(&r).attempt, 1);
}

// ---------------------------------------------------------------- redaction at write time

#[test]
fn secrets_never_reach_the_record_the_accessors_or_the_digest() {
    let session_secret = ["wJalrXUtnFEMI/K7MDENG/", "bPxRfiCYEXAMPLEKEY"].concat();
    let key = key_id();
    let (named, named_value) = named_secret();
    let mut r = Recorder::new(vec![session_secret.clone()]);
    let cmd = format!("curl -H 'x: {session_secret}' {key}");
    r.on_tool_call(&call(
        "t1",
        &format!("run_commands: {cmd}"),
        "execute",
        json!({"commands": [cmd.clone()]}),
    ));
    r.on_tool_call_update(&done(
        "t1",
        "failed",
        shell_result(
            &cmd,
            &format!("[Command exited with code 2]\n{named}\nkey {key}\n{session_secret}"),
            true,
        ),
    ));
    r.on_message(&format!("I found {session_secret} and {key} in the log"));
    let digest = r.stop_digest(1, &facts(1, 0, 1, 1), &ctx()).to_json();
    let everything = format!(
        "{:?}{:?}{:?}{:?}{:?}{:?}{digest}",
        r.calls(),
        r.tool_notes(),
        r.last_messages(),
        r.last_error(),
        r.failed_test_tail(),
        r.anomalies()
    );
    for secret in [session_secret.as_str(), key.as_str(), named_value.as_str()] {
        assert!(
            !everything.contains(secret),
            "{secret} leaked: {everything}"
        );
    }
    assert!(everything.contains("<redacted>"));
}

#[test]
fn a_secret_given_to_the_collector_is_redacted_in_what_the_recorder_keeps() {
    let rec = EventRecorder::new("run-1", "direct");
    let secret = ["S3cretSession", "TokenValue123"].concat();
    let mut c = DirectCollector::new(rec).with_secrets(vec![secret.clone()]);
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            &format!("run_commands: echo {secret}"),
            "execute",
            json!({"commands": [format!("echo {secret}")]}),
        )),
    );
    let notes = c.tool_notes();
    assert_eq!(notes.len(), 1);
    assert!(!notes[0].title.contains(&secret), "{}", notes[0].title);
    assert!(
        !c.stop_digest(1, StopContext::default())
            .to_json()
            .contains(&secret)
    );
}

// ---------------------------------------------------------------- the anomaly lines

#[test]
fn a_blank_key_in_a_tool_input_is_an_anomaly() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: ls",
        "execute",
        json!({" ": "x", "commands": ["ls"]}),
    ));
    let a = r.anomalies();
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0].call, 1);
    assert_eq!(a[0].tool, "run_commands");
    assert!(a[0].line.contains("blank key"), "{}", a[0].line);
}

#[test]
fn an_unknown_key_for_a_known_tool_is_an_anomaly_and_a_known_key_is_not() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: ls",
        "execute",
        json!({"commands": ["ls"], "cwd": "/tmp"}),
    ));
    r.on_tool_call(&call(
        "t2",
        "editor: a.txt",
        "edit",
        json!({"path": "a.txt", "new_text": "x", "old_text": "y", "insert_line": 3}),
    ));
    let a = r.anomalies();
    assert_eq!(a.len(), 1, "{a:?}");
    assert!(a[0].line.contains("unknown key \"cwd\""), "{}", a[0].line);
}

#[test]
fn a_tool_the_harness_does_not_know_is_checked_for_blank_keys_only() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "frobnicate: x",
        "other",
        json!({"whatever": 1}),
    ));
    assert!(r.anomalies().is_empty());
    r.on_tool_call(&call("t2", "frobnicate: y", "other", json!({"": 1})));
    assert_eq!(r.anomalies().len(), 1);
}

#[test]
fn an_input_that_is_not_an_object_or_is_empty_is_an_anomaly() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call("t1", "run_commands: ls", "execute", json!("ls -la")));
    r.on_tool_call(&call("t2", "run_commands: ls", "execute", json!({})));
    let a = r.anomalies();
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(a[0].line.contains("not an object"), "{}", a[0].line);
    assert!(a[1].line.contains("empty input"), "{}", a[1].line);
}

#[test]
fn only_the_newest_anomaly_lines_are_kept() {
    let mut r = Recorder::new(vec![]);
    for i in 0..30 {
        r.on_tool_call(&call(
            &format!("t{i}"),
            "run_commands: ls",
            "execute",
            json!({" ": "x", "commands": ["ls"]}),
        ));
    }
    let a = r.anomalies();
    assert!(a.len() <= 16, "{} lines", a.len());
    assert_eq!(a.iter().map(|x| x.call).max(), Some(30), "the newest stays");
}

// ---------------------------------------------------------------- messages, counters, tails

#[test]
fn the_last_three_messages_are_kept_oldest_first_and_bounded() {
    let mut r = Recorder::new(vec![]);
    for m in ["one", "two", "three", "four"] {
        r.on_message(m);
    }
    assert_eq!(r.last_messages(), vec!["two", "three", "four"]);
    assert_eq!(MESSAGES_KEPT, 3);
    r.on_message(&"m".repeat(20_000));
    let last = r.last_messages().pop().unwrap();
    assert!(last.len() <= MESSAGE_BYTES, "{} bytes", last.len());
}

#[test]
fn a_blank_message_is_not_kept() {
    let mut r = Recorder::new(vec![]);
    r.on_message("real");
    r.on_message("  \n ");
    assert_eq!(r.last_messages(), vec!["real"]);
}

#[test]
fn calls_with_the_same_input_in_a_row_are_counted_by_input_not_by_title() {
    let mut r = Recorder::new(vec![]);
    for i in 0..4 {
        // different titles (a counter in the cut title), the same input
        r.on_tool_call(&call(
            &format!("t{i}"),
            &format!("run_commands: sleep 25 # {i}"),
            "execute",
            json!({"commands": ["sleep 25"]}),
        ));
    }
    r.on_tool_call(&call(
        "t9",
        "run_commands: ls",
        "execute",
        json!({"commands": ["ls"]}),
    ));
    assert_eq!(r.counters().same_input_tail, 1);
    r.on_tool_call(&call(
        "t10",
        "run_commands: ls",
        "execute",
        json!({"commands": ["ls"]}),
    ));
    assert_eq!(r.counters().same_input_tail, 2);
}

#[test]
fn failed_and_timed_out_calls_in_a_row_are_counted_by_the_recorded_verdict() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "ok", "fine");
    run_cmd(&mut r, "t2", "bad", "[Command exited with code 1]");
    run_cmd(&mut r, "t3", "slow", "Command timed out after 30000ms");
    assert_eq!(
        r.counters().bad_tail,
        2,
        "the engine said completed three times"
    );
    run_cmd(&mut r, "t4", "ok2", "fine");
    assert_eq!(r.counters().bad_tail, 0);
}

#[test]
fn the_tail_of_the_last_failed_test_run_is_kept_until_a_later_test_run_passes() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "ls", "files");
    assert_eq!(r.failed_test_tail(), None, "no test run yet");
    let out = format!(
        "[Command exited with code 1]\n{}\nFAILED tests/test_a.py::test_a",
        "noise\n".repeat(2_000)
    );
    run_cmd(&mut r, "t2", "python -m pytest -q", &out);
    let tail = r.failed_test_tail().unwrap();
    assert!(tail.len() <= TEST_TAIL_BYTES, "{} bytes", tail.len());
    assert!(tail.contains("FAILED tests/test_a.py::test_a"));
    run_cmd(&mut r, "t3", "cargo test -p agentd", "test result: ok");
    assert_eq!(r.failed_test_tail(), None, "a later test run ended well");
}

#[test]
fn a_failed_call_that_is_not_a_test_run_leaves_no_test_tail() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "make deploy", "[Command exited with code 2]");
    assert_eq!(r.failed_test_tail(), None);
}

#[test]
fn last_error_is_the_newest_failed_calls_error_on_one_line() {
    let mut r = Recorder::new(vec![]);
    assert_eq!(r.last_error(), None);
    r.on_tool_call(&call("t1", "read_files: a", "read", json!({"files": []})));
    r.on_tool_call_update(&done("t1", "failed", json!("first\nerror")));
    r.on_tool_call(&call("t2", "read_files: b", "read", json!({"files": []})));
    r.on_tool_call_update(&done(
        "t2",
        "failed",
        json!(format!("second\n{}", "z".repeat(500))),
    ));
    let e = r.last_error().unwrap();
    assert!(e.starts_with("second "), "{e}");
    assert!(!e.contains('\n'));
    assert!(e.chars().count() <= LAST_ERROR_CHARS);
}

// ---------------------------------------------------------------- the digest of a stop

#[test]
fn the_digest_carries_the_harness_facts_the_engine_counters_and_the_record() {
    let mut r = Recorder::new(vec![]);
    for i in 1..=3 {
        run_cmd(&mut r, &format!("t{i}"), &format!("echo {i}"), "ok");
    }
    r.on_message("working on it");
    let mut c = ctx();
    c.final_message = Some("I will keep polling the build.".into());
    let d = r.stop_digest(2, &facts(7, 5, 1, 5), &c);
    let v = d.to_value();
    assert_eq!(v["n"], 2);
    assert_eq!(v["schema"], 1);
    assert_eq!(v["harness"]["outcome"], "stopped_engine_cancelled");
    assert_eq!(v["harness"]["stopReason"], "cancelled");
    assert_eq!(v["harness"]["session"], 1);
    assert_eq!(v["harness"]["runS"], 412);
    assert_eq!(v["harness"]["filesChanged"], 3);
    assert_eq!(v["engineCounters"]["identicalTail"], 5);
    assert_eq!(v["engineCounters"]["failedTail"], 1);
    assert_eq!(v["engineCounters"]["toolCalls"], 7);
    assert_eq!(v["recorderCounters"]["calls"], 3);
    assert_eq!(v["calls"].as_array().unwrap().len(), 3);
    assert_eq!(v["droppedCalls"], 0);
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(
        messages.last().unwrap(),
        "I will keep polling the build.",
        "the message being written counts as the newest"
    );
    assert!(messages.iter().any(|m| m == "working on it"));
}

#[test]
fn the_digest_never_exceeds_its_limit_keeps_the_newest_calls_whole_and_says_what_it_left_out() {
    let mut r = Recorder::new(vec![]);
    let big_out = format!(
        "[Command exited with code 1]\n{}",
        "output line\n".repeat(6_000)
    );
    for i in 1..=45 {
        let input = json!({" ": "x", "commands": [format!("cmd {i} {}", "y".repeat(20_000))]});
        r.on_tool_call(&call(
            &format!("t{i}"),
            &format!("run_commands: cmd {i}"),
            "execute",
            input,
        ));
        r.on_tool_call_update(&done(
            &format!("t{i}"),
            "completed",
            shell_result("cmd", &big_out, true),
        ));
        r.on_message(&"m".repeat(100_000));
    }
    let mut c = ctx();
    c.detail = Some("d".repeat(20_000));
    let d = r.stop_digest(1, &facts(45, 0, 45, 1), &c);
    let json = d.to_json();
    assert!(
        json.len() <= STOP_DIGEST_MAX_BYTES,
        "{} bytes over the limit",
        json.len()
    );
    let v: Value = serde_json::from_str(&json).unwrap();
    let calls = v["calls"].as_array().unwrap();
    let dropped = v["droppedCalls"].as_u64().unwrap();
    assert!(dropped > 0, "40 calls of this size cannot fit in 256 KiB");
    assert_eq!(
        calls.len() as u64 + dropped,
        RING_CALLS as u64,
        "every call in the ring is either kept or counted as left out"
    );
    assert_eq!(
        calls.last().unwrap()["n"],
        45,
        "the newest call is always kept"
    );
    // the kept calls are a suffix of the ring: consecutive numbers up to the newest
    let ns: Vec<u64> = calls.iter().map(|c| c["n"].as_u64().unwrap()).collect();
    assert!(ns.windows(2).all(|w| w[1] == w[0] + 1), "{ns:?}");
}

#[test]
fn a_small_run_is_never_cut() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "ls", "a\nb");
    let d = r.stop_digest(1, &facts(1, 1, 0, 1), &ctx());
    assert_eq!(d.to_value()["droppedCalls"], 0);
    assert!(d.to_json().len() < 4_000);
}

#[test]
fn the_digest_json_is_valid_with_four_byte_characters_everywhere() {
    let mut r = Recorder::new(vec![]);
    let g = "\u{1D11E}".repeat(3_000);
    for i in 1..=40 {
        r.on_tool_call(&call(
            &format!("t{i}"),
            &format!("run_commands: {g}"),
            "execute",
            json!({"commands": [g.clone()]}),
        ));
        r.on_tool_call_update(&done(&format!("t{i}"), "failed", Value::String(g.clone())));
        r.on_message(&g);
    }
    let mut c = ctx();
    c.detail = Some(g.clone());
    let json = r.stop_digest(1, &facts(40, 0, 40, 1), &c).to_json();
    assert!(json.len() <= STOP_DIGEST_MAX_BYTES, "{} bytes", json.len());
    serde_json::from_str::<Value>(&json).unwrap();
}

// ---------------------------------------------------------------- the collector feeds the recorder

#[tokio::test(start_paused = true)]
async fn the_collector_lists_the_last_tool_calls_with_the_recorders_reading_of_each() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec);
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            "run_commands: pytest",
            "execute",
            json!({"commands": ["pytest"]}),
        )),
    );
    c.on_notification(
        "session/update",
        &update(done(
            "t1",
            "completed",
            shell_result("pytest", "[Command exited with code 1]", true),
        )),
    );
    let notes = c.tool_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "run_commands: pytest");
    assert_eq!(notes[0].status, "failed");
    assert_eq!(
        c.last_error(),
        Some("[Command exited with code 1]".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn the_collector_lists_the_last_three_messages_with_the_one_being_written_as_the_newest() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec);
    c.on_notification("session/update", &chunk("one"));
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            "run_commands: a",
            "execute",
            json!({"commands": ["a"]}),
        )),
    );
    c.on_notification("session/update", &chunk("two"));
    c.on_notification(
        "session/update",
        &update(call(
            "t2",
            "run_commands: b",
            "execute",
            json!({"commands": ["b"]}),
        )),
    );
    c.on_notification("session/update", &chunk("three"));
    assert_eq!(c.last_messages(), vec!["one", "two", "three"]);
}

#[tokio::test(start_paused = true)]
async fn the_collectors_stop_digest_carries_its_attempt_facts_and_the_final_message() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec);
    for i in 0..5 {
        c.on_notification(
            "session/update",
            &update(call(
                &format!("t{i}"),
                "run_commands: sleep 25",
                "execute",
                json!({"commands": ["sleep 25"]}),
            )),
        );
        c.on_notification(
            "session/update",
            &update(done(
                &format!("t{i}"),
                "completed",
                shell_result("sleep 25", "ok", true),
            )),
        );
    }
    c.on_notification("session/update", &chunk("Still waiting for the build."));
    let v = c.stop_digest(1, StopContext::default()).to_value();
    assert_eq!(
        v["engineCounters"]["identicalTail"], 5,
        "PM3's counters, not recomputed"
    );
    assert_eq!(v["engineCounters"]["toolCalls"], 5);
    assert_eq!(v["recorderCounters"]["sameInputTail"], 5);
    assert_eq!(
        v["messages"].as_array().unwrap().last().unwrap(),
        "Still waiting for the build."
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_prompt_numbers_the_next_attempt_in_the_record() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec);
    c.begin_prompt();
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            "run_commands: a",
            "execute",
            json!({"commands": ["a"]}),
        )),
    );
    c.begin_prompt();
    c.on_notification(
        "session/update",
        &update(call(
            "t2",
            "run_commands: b",
            "execute",
            json!({"commands": ["b"]}),
        )),
    );
    let v = c.stop_digest(2, StopContext::default()).to_value();
    let calls = v["calls"].as_array().unwrap();
    assert_eq!(calls[0]["attempt"], 1);
    assert_eq!(calls[1]["attempt"], 2);
}

#[tokio::test(start_paused = true)]
async fn a_needs_input_event_carries_the_request_id_as_its_event_id() {
    let rec = EventRecorder::new("run-1", "direct");
    let c = DirectCollector::new(rec.clone());
    let id = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
    c.record_needs_input(
        id,
        json!({"turn": 1, "kind": "needs-input", "requestId": id, "reason": "engine_stop"}),
    );
    let all = rec.drain();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].event_id, id);
    assert_eq!(all[0].event_type, EventType::TurnProgress);
    match &all[0].payload {
        Payload::Inline { value } => assert_eq!(value["reason"], "engine_stop"),
        other => panic!("not inline: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn the_outage_notice_is_a_non_blocking_needs_input_with_no_window_and_no_keys() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec.clone());
    c.begin_prompt();
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            "run_commands: a",
            "execute",
            json!({"commands": ["a"]}),
        )),
    );
    c.begin_prompt();
    rec.drain(); // the tool.summary of the call above
    c.record_outage_notice(1_860);
    let all = rec.drain();
    assert_eq!(all.len(), 1);
    let v = match &all[0].payload {
        Payload::Inline { value } => value.clone(),
        other => panic!("not inline: {other:?}"),
    };
    assert_eq!(all[0].event_type, EventType::TurnProgress);
    assert_eq!(v["kind"], "needs-input");
    assert_eq!(v["reason"], "provider_outage");
    assert_eq!(v["blocking"], false);
    assert_eq!(v["options"], json!([]));
    assert_eq!(v["models"], json!([]));
    assert_eq!(v["waitS"], 0);
    assert_eq!(v["expiresAt"], Value::Null);
    assert_eq!(v["canContinue"], false);
    assert_eq!(v["title"], "Every model route is down");
    assert!(v["text"].as_str().unwrap().contains("31 minutes"), "{v}");
    assert!(v["text"].as_str().unwrap().chars().count() <= 160);
    assert_eq!(v["evidence"]["toolCalls"], 1);
    assert_eq!(v["evidence"]["attempts"], 2, "two prompts were sent");
    assert_eq!(v["evidence"]["sessions"], 1);
    assert_eq!(
        v["requestId"].as_str().unwrap(),
        all[0].event_id,
        "the event id IS the request id"
    );
    assert_eq!(all[0].event_id.len(), 26, "a ULID");
    // it is exactly the payload the protocol defines, and no stand-in member rides along
    let parsed: harness_protocol::direct::NeedsInputPayload =
        serde_json::from_value(v.clone()).unwrap();
    assert!(!parsed.blocking);
    assert!(v.get("outageMinutes").is_none());
}

#[tokio::test(start_paused = true)]
async fn the_outage_notice_counts_the_session_it_is_told() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec.clone());
    c.set_session(2);
    c.record_outage_notice(1_800);
    let all = rec.drain();
    match &all[0].payload {
        Payload::Inline { value } => assert_eq!(value["evidence"]["sessions"], 2),
        other => panic!("not inline: {other:?}"),
    }
}

#[test]
fn an_event_can_carry_an_id_chosen_by_the_caller_and_the_normal_ids_stay_ulids() {
    let rec = EventRecorder::new("run-1", "direct");
    rec.record(EventType::TurnProgress, json!({"a": 1}));
    rec.record_urgent_with_id(
        EventType::TurnProgress,
        "01M5A2R7K3C9XJ4D8E6FQ0WT1N",
        json!({"b": 2}),
    );
    let all = rec.drain();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].event_id.len(), 26);
    assert_ne!(all[0].event_id, "01M5A2R7K3C9XJ4D8E6FQ0WT1N");
    assert_eq!(all[1].event_id, "01M5A2R7K3C9XJ4D8E6FQ0WT1N");
    assert_eq!(all[1].seq, all[0].seq + 1, "one sequence for both");
}

// ---------------------------------------------------------------- the shapes Cline really sends (S0, 2026-10-15)

#[test]
fn a_failing_edit_the_engine_reports_completed_reads_failed_from_its_json_error() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "editor: README.md",
        "edit",
        json!({"path": "README.md", "old_text": "zzz-1", "new_text": "x"}),
    ));
    r.on_tool_call_update(&done(
        "t1",
        "completed",
        json!({"success": false, "error": "old_text not found in README.md"}),
    ));
    let c = only_call(&r);
    assert_eq!(c.engine_status, "completed");
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert!(c.error.unwrap().contains("old_text not found"));
    assert_eq!(r.tool_notes()[0].status, "failed");
}

#[test]
fn a_failing_read_the_engine_reports_completed_reads_failed_from_the_item_errors() {
    let mut r = Recorder::new(vec![]);
    for i in 0..8 {
        let id = format!("t{i}");
        r.on_tool_call(&call(
            &id,
            "read_files: nope.txt",
            "read",
            json!({"files": [{"path": format!("nope-{i}.txt")}]}),
        ));
        r.on_tool_call_update(&done(
            &id,
            "completed",
            Value::String(
                json!([{"path": "nope.txt", "error": "ENOENT: no such file or directory"}])
                    .to_string(),
            ),
        ));
    }
    assert!(r.calls().iter().all(|c| c.verdict == CallVerdict::Failed));
    assert_eq!(
        r.counters().bad_tail,
        8,
        "the engine's own failed counter stays at zero for these; the recorder sees them"
    );
}

#[test]
fn plain_text_starting_with_error_colon_reads_failed_but_text_that_mentions_errors_does_not() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "editor: a",
        "edit",
        json!({"path": "a", "new_text": "b"}),
    ));
    r.on_tool_call_update(&done("t1", "completed", json!("Error: file is read-only")));
    r.on_tool_call(&call("t2", "read_files: b", "read", json!({"files": []})));
    r.on_tool_call_update(&done(
        "t2",
        "completed",
        json!("Error handling is described below. The word error: appears in prose."),
    ));
    let calls = r.calls();
    assert_eq!(calls[0].verdict, CallVerdict::Failed);
    assert_eq!(calls[1].verdict, CallVerdict::Ok);
}

#[test]
fn output_in_a_content_array_is_read_like_raw_output() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: make",
        "execute",
        json!({"commands": ["make"]}),
    ));
    r.on_tool_call_update(&json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": "[Command exited with code 2]\nmake: boom"}}]
    }));
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert_eq!(c.exit_code, Some(2));
    assert!(c.output_tail.contains("make: boom"));
}

#[test]
fn a_diff_content_keeps_the_path_and_never_the_text_of_the_file() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "Edit src/calc.py",
        "edit",
        json!({"path": "src/calc.py"}),
    ));
    r.on_tool_call_update(&json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
        "content": [{"type": "diff", "path": "src/calc.py", "oldText": "OLD-FILE-TEXT", "newText": "NEW-FILE-TEXT"}]
    }));
    let c = only_call(&r);
    assert!(c.output_tail.contains("src/calc.py"), "{}", c.output_tail);
    assert!(!c.output_tail.contains("NEW-FILE-TEXT"));
    assert!(!c.output_tail.contains("OLD-FILE-TEXT"));
    assert_eq!(c.verdict, CallVerdict::Ok);
}

// ---------------------------------------------------------------- more redaction and anomaly rows

#[test]
fn name_value_secrets_in_json_and_shell_forms_and_bearer_tokens_are_redacted() {
    // built at run time: no secret-shaped literal sits in the repository
    let value = ["v4lu3", "v4lu3-xyz"].concat();
    let value = value.as_str();
    let name_pw = ["pass", "word"].concat();
    let forms = [
        format!("export API_KEY={value}"),
        format!("{{\"{name_pw}\": \"{value}\"}}"),
        format!("{name_pw}: {value}"),
        format!("SECRET_TOKEN='{value}'"),
        format!("Authorization: Bearer {value}{value}"),
    ];
    let mut r = Recorder::new(vec![]);
    for (i, f) in forms.iter().enumerate() {
        run_cmd(&mut r, &format!("t{i}"), "env", f);
    }
    for c in r.calls() {
        assert!(!c.output_tail.contains(value), "{}", c.output_tail);
        assert!(c.output_tail.contains("<redacted>"), "{}", c.output_tail);
    }
}

#[test]
fn a_short_value_or_a_name_inside_a_longer_word_is_not_redacted() {
    let mut r = Recorder::new(vec![]);
    run_cmd(
        &mut r,
        "t1",
        "env",
        "tokenizer=fastpath1\nkey_count=3\nPASSWORD_FILE=/run/secrets/db",
    );
    let c = only_call(&r);
    assert!(
        c.output_tail.contains("tokenizer=fastpath1"),
        "{}",
        c.output_tail
    );
    assert!(c.output_tail.contains("PASSWORD_FILE=/run/secrets/db"));
    assert!(!c.output_tail.contains("<redacted>"));
}

#[test]
fn a_known_tool_without_its_required_key_is_an_anomaly() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: ls",
        "execute",
        json!({"cmd": "ls"}),
    ));
    let lines: Vec<String> = r.anomalies().into_iter().map(|a| a.line).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("unknown key \"cmd\"")));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("missing required key \"commands\""))
    );
}

#[test]
fn test_runs_are_recognised_by_their_command() {
    for cmd in [
        "go test ./...",
        "npm test",
        "npm run test -- --watch=false",
        "mvn -q test",
        "python -m unittest discover",
        "cargo nextest run -p agentd",
        "npx jest --ci",
        "./gradlew test",
    ] {
        let mut r = Recorder::new(vec![]);
        run_cmd(&mut r, "t1", cmd, "[Command exited with code 1]\nFAIL one");
        assert!(r.failed_test_tail().is_some(), "{cmd} is a test run");
    }
    for cmd in ["ls -la", "git status", "make build", "cat tests/test_a.py"] {
        let mut r = Recorder::new(vec![]);
        run_cmd(&mut r, "t1", cmd, "[Command exited with code 1]\nFAIL one");
        assert!(r.failed_test_tail().is_none(), "{cmd} is not a test run");
    }
}

#[test]
fn older_calls_that_do_not_fit_whole_are_kept_in_compact_form_before_any_is_dropped() {
    let mut r = Recorder::new(vec![]);
    let out = format!("[Command exited with code 1]\n{}", "x".repeat(6_000));
    for i in 1..=40 {
        let big = "y".repeat(2_000);
        r.on_tool_call(&call(
            &format!("t{i}"),
            &format!("run_commands: step {i}"),
            "execute",
            json!({"commands": [format!("step {i} {big}")]}),
        ));
        r.on_tool_call_update(&done(
            &format!("t{i}"),
            "completed",
            shell_result("s", &out, true),
        ));
    }
    let d = r.stop_digest(1, &facts(40, 40, 0, 1), &ctx());
    let v = d.to_value();
    let calls = v["calls"].as_array().unwrap();
    assert!(
        calls.len() >= 25,
        "compact records keep most of the 40 calls: {}",
        calls.len()
    );
    assert_eq!(calls.len() as u64 + v["droppedCalls"].as_u64().unwrap(), 40);
    let oldest = &calls[0];
    assert!(oldest["inputHead"].as_str().unwrap().len() <= COMPACT_FIELD_BYTES);
    assert!(oldest["outputTail"].as_str().unwrap().len() <= COMPACT_FIELD_BYTES);
    let newest = calls.last().unwrap();
    assert!(
        newest["outputTail"].as_str().unwrap().len() > COMPACT_FIELD_BYTES,
        "the newest is whole"
    );
    assert!(d.to_json().len() <= STOP_DIGEST_MAX_BYTES);
}

// ---------------------------------------------------------------- edges of the readers and of the budget

#[test]
fn a_tail_cut_inside_a_four_byte_character_starts_on_the_next_boundary() {
    let mut r = Recorder::new(vec![]);
    run_cmd(&mut r, "t1", "cat big", &"\u{1D11E}".repeat(20_000));
    let c = only_call(&r);
    assert!(c.output_tail.len() <= OUTPUT_TAIL_BYTES);
    assert!(c.output_tail.len() > OUTPUT_TAIL_BYTES - 4);
    assert!(c.output_tail.chars().all(|ch| ch == '\u{1D11E}'));
}

#[test]
fn spaces_before_the_separator_and_overlapping_matches_are_redacted_once() {
    // The values are built at run time: no secret-shaped literal sits in the repository.
    let v1 = ["abcdef", "123456"].concat();
    let v2 = ["zyxwvu", "654321"].concat();
    let v3 = ["abcdefghij", "klmnop1234"].concat();
    let mut r = Recorder::new(vec![]);
    run_cmd(
        &mut r,
        "t1",
        "env",
        &format!("token  =  {v1} and API_KEY :  {v2}"),
    );
    run_cmd(
        &mut r,
        "t2",
        "env",
        &format!("Authorization: Bearer token={v3}"),
    );
    for c in r.calls() {
        assert!(!c.output_tail.contains(&v1), "{}", c.output_tail);
        assert!(!c.output_tail.contains(&v2), "{}", c.output_tail);
        assert!(!c.output_tail.contains(&v3), "{}", c.output_tail);
        assert!(c.output_tail.contains("<redacted>"));
    }
}

#[test]
fn an_exit_marker_that_is_not_a_number_is_ignored() {
    let mut r = Recorder::new(vec![]);
    run_cmd(
        &mut r,
        "t1",
        "weird",
        "[Command exited with code abc]\nstill fine",
    );
    let c = only_call(&r);
    assert_eq!(c.exit_code, None);
    assert_eq!(c.verdict, CallVerdict::Ok);
}

#[test]
fn result_items_of_every_odd_shape_are_read_without_failing() {
    let mut r = Recorder::new(vec![]);
    let shapes = [
        json!([null, "plain text item"]),
        json!(["a", "b"]),
        json!([{"error": "first", "path": "a"}, {"error": "second", "path": "b"}]),
        json!([{"path": "a.txt", "lines": 3}]),
        json!(42),
        json!(true),
    ];
    for (i, raw) in shapes.iter().enumerate() {
        let id = format!("t{i}");
        r.on_tool_call(&call(&id, "read_files: x", "read", json!({"files": []})));
        r.on_tool_call_update(&done(&id, "completed", raw.clone()));
    }
    let calls = r.calls();
    assert_eq!(calls.len(), 6);
    assert!(calls[0].output_tail.contains("plain text item"));
    assert!(calls[1].output_tail.contains('a') && calls[1].output_tail.contains('b'));
    let two = calls[2].error.clone().unwrap();
    assert!(two.contains("first") && two.contains("second"), "{two}");
    assert_eq!(calls[2].verdict, CallVerdict::Failed);
    assert!(
        calls[3].output_tail.contains("lines"),
        "{}",
        calls[3].output_tail
    );
    assert_eq!(calls[4].output_tail, "42");
    assert_eq!(calls[5].output_tail, "true");
    assert!(calls[3..].iter().all(|c| c.verdict == CallVerdict::Ok));
}

#[test]
fn a_null_content_and_an_update_without_an_id_change_nothing() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: a",
        "execute",
        json!({"commands": ["a"]}),
    ));
    r.on_tool_call_update(&json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": null
    }));
    r.on_tool_call_update(&json!({"sessionUpdate": "tool_call_update", "status": "failed"}));
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Ok);
    assert_eq!(c.output_tail, "");
}

#[test]
fn the_final_status_alone_settles_a_call_whose_output_came_in_an_earlier_update() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: pytest -q",
        "execute",
        json!({"commands": ["pytest -q"]}),
    ));
    r.on_tool_call_update(&json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress",
        "rawOutput": "FAILED tests/test_a.py::test_a"
    }));
    assert_eq!(
        only_call(&r).verdict,
        CallVerdict::NoResult,
        "not final yet"
    );
    r.on_tool_call_update(
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"}),
    );
    let c = only_call(&r);
    assert_eq!(c.verdict, CallVerdict::Failed);
    assert!(
        r.failed_test_tail()
            .unwrap()
            .contains("FAILED tests/test_a.py::test_a")
    );
    // and a failed call with no output at all still says why
    r.on_tool_call(&call("t2", "read_files: x", "read", json!({"files": []})));
    r.on_tool_call_update(
        &json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "failed"}),
    );
    assert_eq!(
        r.calls()[1].error.as_deref(),
        Some("the engine reported the call failed")
    );
}

#[test]
fn several_blank_keys_and_inputs_of_other_json_types_are_anomalies() {
    let mut r = Recorder::new(vec![]);
    r.on_tool_call(&call(
        "t1",
        "run_commands: ls",
        "execute",
        json!({" ": 1, "": 2, "commands": ["ls"]}),
    ));
    r.on_tool_call(&call("t2", "run_commands: ls", "execute", json!(["ls"])));
    r.on_tool_call(&call("t3", "run_commands: ls", "execute", json!(5)));
    r.on_tool_call(&call("t4", "run_commands: ls", "execute", json!(true)));
    let lines: Vec<String> = r.anomalies().into_iter().map(|a| a.line).collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].contains("2 blank keys"), "{}", lines[0]);
    assert!(lines[1].contains("an array"), "{}", lines[1]);
    assert!(lines[2].contains("a number"), "{}", lines[2]);
    assert!(lines[3].contains("a boolean"), "{}", lines[3]);
}

#[test]
fn a_recorder_that_saw_nothing_has_zero_counters() {
    let r = Recorder::new(vec![]);
    assert_eq!(r.counters().calls, 0);
    assert_eq!(r.counters().same_input_tail, 0);
    assert_eq!(r.counters().bad_tail, 0);
    assert!(r.calls().is_empty() && r.tool_notes().is_empty() && r.anomalies().is_empty());
    let d = r.stop_digest(1, &facts(0, 0, 0, 0), &StopContext::default());
    assert_eq!(
        d.to_value()["harness"]["session"],
        1,
        "a session is at least the first"
    );
}

#[test]
fn a_smaller_limit_drops_the_messages_then_the_test_tail_then_the_anomalies_then_the_calls() {
    let mut r = Recorder::new(vec![]);
    for i in 1..=6 {
        r.on_tool_call(&call(
            &format!("t{i}"),
            "run_commands: cargo test",
            "execute",
            json!({" ": 1, "commands": ["cargo test"]}),
        ));
        r.on_tool_call_update(&done(
            &format!("t{i}"),
            "completed",
            shell_result(
                "cargo test",
                &format!("[Command exited with code 101]\n{}", "e".repeat(2_500)),
                true,
            ),
        ));
        r.on_message(&format!("{i}{}", "m".repeat(7_000)));
    }
    let mut c = ctx();
    c.final_message = Some("the newest words".into());
    let full = r.stop_digest(1, &facts(6, 0, 6, 1), &c).to_json().len();
    // a limit under the fixed part: messages go oldest first and the newest stays while it can
    let mid = r.stop_digest_within(1, &facts(6, 0, 6, 1), &c, 15_000);
    assert!(
        mid.to_json().len() <= 15_000,
        "{} bytes",
        mid.to_json().len()
    );
    assert!(mid.messages.len() < 3, "{} messages", mid.messages.len());
    assert_eq!(mid.messages.last().unwrap(), "the newest words");
    // a very small limit leaves the harness facts and, at most, the newest anomaly lines
    let tiny = r.stop_digest_within(1, &facts(6, 0, 6, 1), &c, 800);
    assert!(
        tiny.to_json().len() <= 800,
        "{} bytes",
        tiny.to_json().len()
    );
    assert!(tiny.messages.is_empty() && tiny.calls.is_empty());
    assert!(tiny.failed_test_tail.is_none());
    assert!(
        tiny.anomalies.len() < r.anomalies().len(),
        "the anomaly lines shrink last but one"
    );
    assert_eq!(
        tiny.to_value()["harness"]["outcome"],
        "stopped_engine_cancelled"
    );
    // calls go last: with room for the fixed part and one call, the newest call survives
    let one = r.stop_digest_within(1, &facts(6, 0, 6, 1), &c, full - 12_000);
    assert!(one.to_json().len() <= full - 12_000);
}

#[tokio::test(start_paused = true)]
async fn the_collectors_accessors_read_the_same_record() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c = DirectCollector::new(rec);
    c.begin_prompt();
    c.on_notification(
        "session/update",
        &update(call(
            "t1",
            "run_commands: pytest",
            "execute",
            json!({" ": 1, "commands": ["pytest"]}),
        )),
    );
    c.on_notification(
        "session/update",
        &update(done(
            "t1",
            "completed",
            shell_result("pytest", "[Command exited with code 1]\nFAILED t", true),
        )),
    );
    assert!(c.failed_test_tail().unwrap().contains("FAILED t"));
    assert_eq!(c.anomalies().len(), 1);
    let counters = c.recorder_counters();
    assert_eq!(counters.calls, 1);
    assert_eq!(counters.bad_tail, 1);
}
