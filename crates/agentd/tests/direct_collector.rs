//! The collector of the direct driver: it records what Cline does and never decides. Message
//! chunks become the final message, tool calls become `tool.summary` events, a progress line goes out at most once per 5 s,
//! and a permission request (none is expected with `--auto-approve true`) is answered allow, never refused.
#![allow(clippy::unwrap_used)]

use agentd::acp::AcpHandler;
use agentd::direct::collector::DirectCollector;
use agentd::events::EventRecorder;
use harness_protocol::envelope::{EventType, Payload};
use serde_json::{Value, json};
use std::time::Duration;

fn update(u: Value) -> Value {
    json!({"sessionId": "s1", "update": u})
}

fn chunk(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn tool_call(id: &str, title: &str) -> Value {
    update(
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "edit", "status": "pending"}),
    )
}

fn events(rec: &EventRecorder) -> Vec<(EventType, Value)> {
    rec.drain()
        .into_iter()
        .map(|e| match e.payload {
            Payload::Inline { value } => (e.event_type, value),
            other => panic!("not inline: {other:?}"),
        })
        .collect()
}

fn collector() -> (DirectCollector, std::sync::Arc<EventRecorder>) {
    let rec = EventRecorder::new("run-1", "direct");
    (DirectCollector::new(rec.clone()), rec)
}

#[tokio::test(start_paused = true)]
async fn message_chunks_join_into_the_final_message() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &chunk("Added "));
    c.on_notification("session/update", &chunk("percent_change."));
    assert_eq!(c.final_message(), "Added percent_change.");
}

#[tokio::test(start_paused = true)]
async fn the_final_message_is_the_text_after_the_last_tool_call() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &chunk("I will edit the file."));
    c.on_notification("session/update", &tool_call("t1", "Edit src/calc.py"));
    c.on_notification("session/update", &chunk("Done: added the function."));
    assert_eq!(c.final_message(), "Done: added the function.");
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_ends_on_a_tool_call_keeps_the_last_words_before_it() {
    // Cline may finish with a tool call that carries no further text; its last words still stand.
    let (mut c, _) = collector();
    c.on_notification("session/update", &chunk("Working on it."));
    c.on_notification("session/update", &tool_call("t1", "Edit src/calc.py"));
    assert_eq!(c.final_message(), "Working on it.");
}

#[tokio::test(start_paused = true)]
async fn a_turn_with_no_text_at_all_has_no_final_message() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &tool_call("t1", "Edit src/calc.py"));
    assert_eq!(c.final_message(), "");
}

#[tokio::test(start_paused = true)]
async fn control_characters_are_removed_from_the_final_message() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &chunk("ok\u{0}\u{7}\nnext"));
    assert_eq!(c.final_message(), "ok\nnext");
}

#[tokio::test(start_paused = true)]
async fn each_tool_call_is_counted_and_recorded_as_a_tool_summary() {
    let (mut c, rec) = collector();
    c.on_notification("session/update", &tool_call("t1", "Edit src/calc.py"));
    c.on_notification("session/update", &tool_call("t2", "Run tests"));
    assert_eq!(c.tool_calls(), 2);
    let ev: Vec<_> = events(&rec)
        .into_iter()
        .filter(|(t, _)| *t == EventType::ToolSummary)
        .collect();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].1["toolCallId"], "t1");
    assert_eq!(ev[0].1["title"], "Edit src/calc.py");
    assert_eq!(ev[0].1["kind"], "edit");
    assert_eq!(ev[0].1["status"], "pending");
}

#[tokio::test(start_paused = true)]
async fn a_tool_call_update_is_recorded_as_turn_progress() {
    let (mut c, rec) = collector();
    c.on_notification(
        "session/update",
        &update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
        ),
    );
    let ev = events(&rec);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, EventType::TurnProgress);
    assert_eq!(ev[0].1, json!({"toolCallId": "t1", "status": "completed"}));
}

#[tokio::test(start_paused = true)]
async fn the_latest_message_goes_out_as_progress_at_most_once_per_five_seconds() {
    let (mut c, rec) = collector();
    c.on_notification("session/update", &chunk("first"));
    let first = events(&rec);
    assert_eq!(first.len(), 1, "the first message is reported at once");
    assert_eq!(first[0].0, EventType::TurnProgress);
    assert_eq!(first[0].1["kind"], "message");
    assert_eq!(first[0].1["turn"], 1);
    assert_eq!(first[0].1["text"], "first");

    // Inside 5 s: nothing more.
    c.on_notification("session/update", &chunk(" second"));
    tokio::time::advance(Duration::from_secs(4)).await;
    c.on_notification("session/update", &chunk(" third"));
    assert!(events(&rec).is_empty());

    // After 5 s: the text so far, once.
    tokio::time::advance(Duration::from_secs(2)).await;
    c.on_notification("session/update", &chunk("!"));
    let later = events(&rec);
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].1["text"], "first second third!");
}

#[tokio::test(start_paused = true)]
async fn the_progress_text_is_the_last_2000_characters() {
    let (mut c, rec) = collector();
    let long = format!("{}{}", "a".repeat(600), "b".repeat(2_000));
    c.on_notification("session/update", &chunk(&long));
    let ev = events(&rec);
    let text = ev[0].1["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 2_000);
    assert!(text.chars().all(|ch| ch == 'b'));
}

#[tokio::test(start_paused = true)]
async fn usage_in_an_update_is_kept_and_none_stays_none() {
    let (mut c, _) = collector();
    assert_eq!(c.usage(), None);
    c.on_notification(
        "session/update",
        &update(json!({"sessionUpdate": "usage_update", "usage": {"inputTokens": 5, "outputTokens": 7, "cachedReadTokens": 1, "cachedWriteTokens": 2}})),
    );
    let u = c.usage().unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (5, 7));
    assert_eq!(
        (u.cache_read_input_tokens, u.cache_write_input_tokens),
        (1, 2)
    );
}

#[tokio::test(start_paused = true)]
async fn a_permission_request_is_allowed_with_the_first_allow_once_option_and_recorded() {
    let (mut c, rec) = collector();
    let params = json!({
        "sessionId": "s1",
        "toolCall": {"toolCallId": "t9", "title": "run_commands: npm test"},
        "options": [
            {"kind": "reject_once", "name": "Reject", "optionId": "reject_once"},
            {"kind": "allow_always", "name": "Always", "optionId": "allow_always"},
            {"kind": "allow_once", "name": "Allow", "optionId": "allow_once"},
        ],
    });
    let result = c.on_request("session/request_permission", &params).unwrap();
    assert_eq!(result["outcome"]["outcome"], "selected");
    assert_eq!(result["outcome"]["optionId"], "allow_once");
    let ev = events(&rec);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, EventType::PermissionRequested);
    assert_eq!(
        ev[0].1,
        json!({"toolCallId": "t9", "allowed": true, "auto": true, "title": "run_commands: npm test"})
    );
}

#[tokio::test(start_paused = true)]
async fn without_allow_once_it_takes_allow_always() {
    let (mut c, _) = collector();
    let params = json!({
        "toolCall": {"toolCallId": "t9", "title": "x"},
        "options": [
            {"kind": "reject_once", "optionId": "reject_once"},
            {"kind": "allow_always", "optionId": "always"},
        ],
    });
    let result = c.on_request("session/request_permission", &params).unwrap();
    assert_eq!(result["outcome"]["optionId"], "always");
}

#[tokio::test(start_paused = true)]
async fn a_request_with_no_allow_option_is_answered_cancelled_never_refused_by_choice() {
    let (mut c, _) = collector();
    let params = json!({
        "toolCall": {"toolCallId": "t9", "title": "x"},
        "options": [{"kind": "reject_once", "optionId": "reject_once"}],
    });
    let result = c.on_request("session/request_permission", &params).unwrap();
    assert_eq!(result["outcome"]["outcome"], "cancelled");
}

#[tokio::test(start_paused = true)]
async fn any_other_request_is_a_method_not_found_error() {
    let (mut c, _) = collector();
    let err = c.on_request("fs/read_text_file", &json!({})).unwrap_err();
    assert_eq!(err.0, -32601);
}

#[tokio::test(start_paused = true)]
async fn a_notification_that_is_not_a_session_update_changes_nothing() {
    let (mut c, rec) = collector();
    c.on_notification("session/other", &chunk("ignored"));
    assert_eq!(c.final_message(), "");
    assert!(events(&rec).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_new_prompt_forgets_the_last_prompt_s_words_so_a_blank_reply_is_blank() {
    // The final message belongs to the last prompt: a nudge answered with no text must not borrow the first prompt's text.
    let (mut c, _) = collector();
    c.on_notification("session/update", &chunk("I made a start."));
    assert_eq!(c.final_message(), "I made a start.");
    c.begin_prompt();
    assert_eq!(c.final_message(), "");
    c.on_notification("session/update", &chunk("DONE."));
    assert_eq!(c.final_message(), "DONE.");
}

#[tokio::test(start_paused = true)]
async fn secrets_the_collector_was_told_about_never_reach_an_event() {
    let rec = EventRecorder::new("run-1", "direct");
    let mut c =
        DirectCollector::new(rec.clone()).with_secrets(vec!["s3cr3t-value-123456".to_string()]);
    c.on_notification(
        "session/update",
        &chunk("the key is s3cr3t-value-123456 ok"),
    );
    c.on_notification(
        "session/update",
        &tool_call("t1", "run echo s3cr3t-value-123456"),
    );
    let all = serde_json::to_string(&events(&rec)).unwrap();
    assert!(!all.contains("s3cr3t-value-123456"), "{all}");
    assert!(
        !c.final_message().contains("s3cr3t-value-123456"),
        "{}",
        c.final_message()
    );
}

// ---- the facts of one attempt: what Cline's own stops look like from outside ---------------------------------------

fn call(id: &str, title: &str) -> Value {
    update(
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "pending"}),
    )
}

fn finished(id: &str, status: &str) -> Value {
    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status}))
}

#[tokio::test(start_paused = true)]
async fn identical_calls_at_the_tail_are_counted_and_a_different_one_starts_the_count_again() {
    let (mut c, _) = collector();
    for i in 0..5 {
        c.on_notification("session/update", &call(&format!("t{i}"), "sleep 25; date"));
    }
    assert_eq!(c.attempt_facts().identical_tail, 5);
    assert_eq!(c.attempt_facts().tool_calls, 5);
    c.on_notification("session/update", &call("t5", "cat progress.log"));
    assert_eq!(c.attempt_facts().identical_tail, 1);
    c.on_notification("session/update", &call("t6", "cat progress.log"));
    assert_eq!(c.attempt_facts().identical_tail, 2);
    assert_eq!(c.tool_calls(), 7, "the run-wide count is not the attempt's");
}

#[tokio::test(start_paused = true)]
async fn the_kind_is_part_of_what_makes_two_calls_identical() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &call("t1", "run"));
    c.on_notification(
        "session/update",
        &update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "run", "kind": "read", "status": "pending"}),
        ),
    );
    assert_eq!(c.attempt_facts().identical_tail, 1);
}

#[tokio::test(start_paused = true)]
async fn failed_calls_in_a_row_are_counted_and_a_completed_call_ends_the_row() {
    let (mut c, _) = collector();
    for i in 0..6 {
        let id = format!("f{i}");
        c.on_notification("session/update", &call(&id, &format!("read file {i}")));
        c.on_notification("session/update", &finished(&id, "failed"));
    }
    let f = c.attempt_facts();
    assert_eq!((f.failed_tail, f.completed, f.tool_calls), (6, 0, 6));
    c.on_notification("session/update", &call("ok", "read the right file"));
    c.on_notification("session/update", &finished("ok", "completed"));
    let f = c.attempt_facts();
    assert_eq!((f.failed_tail, f.completed), (0, 1));
}

#[tokio::test(start_paused = true)]
async fn a_call_is_resolved_once_whatever_the_engine_repeats() {
    let (mut c, _) = collector();
    c.on_notification("session/update", &call("a", "x"));
    c.on_notification("session/update", &finished("a", "in_progress"));
    c.on_notification("session/update", &finished("a", "failed"));
    c.on_notification("session/update", &finished("a", "failed"));
    c.on_notification("session/update", &finished("a", "completed"));
    let f = c.attempt_facts();
    assert_eq!(f.failed_tail, 1, "a repeated terminal update counts once");
    assert_eq!(f.completed, 0, "the first terminal status stands");
}

#[tokio::test(start_paused = true)]
async fn a_call_that_arrives_already_finished_is_counted_too() {
    let (mut c, _) = collector();
    c.on_notification(
        "session/update",
        &update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "d", "title": "echo", "kind": "execute", "status": "completed"}),
        ),
    );
    c.on_notification(
        "session/update",
        &update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "e", "title": "cat", "kind": "execute", "status": "failed"}),
        ),
    );
    let f = c.attempt_facts();
    assert_eq!((f.completed, f.failed_tail), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn a_new_prompt_starts_the_facts_over() {
    let (mut c, _) = collector();
    for i in 0..3 {
        c.on_notification("session/update", &call(&format!("t{i}"), "same"));
        c.on_notification("session/update", &finished(&format!("t{i}"), "failed"));
    }
    c.begin_prompt();
    let f = c.attempt_facts();
    assert_eq!(
        (f.tool_calls, f.completed, f.failed_tail, f.identical_tail),
        (0, 0, 0, 0)
    );
    c.on_notification("session/update", &call("n1", "same"));
    assert_eq!(
        c.attempt_facts().identical_tail,
        1,
        "the title of the earlier attempt does not carry over"
    );
    assert_eq!(c.tool_calls(), 4, "the run-wide count keeps going");
}

#[tokio::test(start_paused = true)]
async fn titles_are_compared_before_secrets_are_hidden() {
    // Two different commands that both print the same secret must not read as one repeated command.
    let (c, _) = collector();
    // Built at run time: no secret-shaped literal sits in the source.
    let secret = ["sk-live", "0123456789"].join("-");
    let mut c = c.with_secrets(vec![secret.clone()]);
    c.on_notification("session/update", &call("a", &format!("echo {secret} | wc")));
    c.on_notification(
        "session/update",
        &call("b", &format!("echo {secret} | md5")),
    );
    assert_eq!(c.attempt_facts().identical_tail, 1);
}
