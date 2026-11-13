//! The record pipeline of the transcript writer: which ACP message becomes which
//! record, what is never stored, how streamed text is joined and redacted, what a record may weigh, and how the records are numbered.
//! No I/O and no clock of its own: the tests give the pipeline its instants. Every secret-shaped value is built at run time.
#![allow(clippy::unwrap_used)]

use agentd::acp::Dir;
use agentd::direct::transcript::pipeline::{Line, Pipeline, WireMsg};
use agentd::direct::transcript::settings::Settings;
use harness_core::redact::Redactor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const SID: &str = "sess-1";

fn secs(n: f64) -> Duration {
    Duration::from_secs_f64(n)
}

/// A small window so a test needs kilobytes, not megabytes, to reach the cut.
fn small() -> Settings {
    Settings {
        window: 256,
        coalesce_bytes: 1024,
        max_hold: 4096,
        ..Settings::default()
    }
}

struct Rig {
    p: Pipeline,
    r: Redactor,
    t0: Instant,
    now: Instant,
    lines: Vec<Line>,
}

impl Rig {
    fn new(s: Settings) -> Self {
        Self::with(s, Redactor::new(Vec::new()))
    }

    fn with(s: Settings, r: Redactor) -> Self {
        let t0 = Instant::now();
        Self {
            p: Pipeline::new(s, t0),
            r,
            t0,
            now: t0,
            lines: Vec::new(),
        }
    }

    fn at(&mut self, s: f64) -> &mut Self {
        self.now = self.t0 + secs(s);
        self
    }

    fn feed(&mut self, dir: Dir, value: Value) -> &mut Self {
        let out = self.p.feed(
            &self.r,
            self.now,
            WireMsg {
                dir,
                value,
                stale: false,
                at: 1_790_000_000,
            },
        );
        self.lines.extend(out);
        self
    }

    fn stale(&mut self, value: Value) -> &mut Self {
        let out = self.p.feed(
            &self.r,
            self.now,
            WireMsg {
                dir: Dir::FromEngine,
                value,
                stale: true,
                at: 1_790_000_000,
            },
        );
        self.lines.extend(out);
        self
    }

    fn engine(&mut self, value: Value) -> &mut Self {
        self.feed(Dir::FromEngine, value)
    }

    fn agentd(&mut self, value: Value) -> &mut Self {
        self.feed(Dir::ToEngine, value)
    }

    fn tick(&mut self) -> &mut Self {
        let out = self.p.tick(&self.r, self.now);
        self.lines.extend(out);
        self
    }

    fn finish(&mut self) -> &mut Self {
        let out = self.p.finish(&self.r);
        self.lines.extend(out);
        self
    }

    fn records(&self) -> Vec<Value> {
        self.lines
            .iter()
            .map(|l| serde_json::from_slice(&l.bytes).unwrap())
            .collect()
    }

    fn kinds(&self) -> Vec<String> {
        self.records()
            .iter()
            .map(|r| r["kind"].as_str().unwrap().to_string())
            .collect()
    }

    /// The joined text of the message fragments, in order.
    fn message_text(&self) -> String {
        self.records()
            .iter()
            .filter(|r| r["kind"] == "agent_message_chunk")
            .map(|r| {
                r["data"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn all_bytes(&self) -> String {
        self.lines
            .iter()
            .map(|l| String::from_utf8_lossy(&l.bytes).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---- messages ------------------------------------------------------------------------------------------------------------------

fn update(u: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": u}})
}

fn chunk(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn chunk_of(id: &str, text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}}),
    )
}

fn thought(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn tool_call(id: &str, title: &str, input: Value) -> Value {
    update(
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "pending", "rawInput": input}),
    )
}

fn tool_done(id: &str, output: Value) -> Value {
    update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "rawOutput": output}),
    )
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn response(id: u64, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn prompt(id: u64, text: &str) -> Value {
    request(
        id,
        "session/prompt",
        json!({"sessionId": SID, "prompt": [{"type": "text", "text": text}]}),
    )
}

// built at run time: no token-shaped literal is committed
fn github_token(seed: usize) -> String {
    let body: String = "aB3dE5gH7jK9mN1pQ3sT5vW7yZ2cF4hJ6kL8nP0rS2uV4xY6"
        .chars()
        .cycle()
        .skip(seed)
        .take(36)
        .collect();
    format!("gh{}_{body}", 'p')
}

fn filler(n: usize) -> String {
    "the quick brown fox jumps over the lazy dog "
        .chars()
        .cycle()
        .take(n)
        .collect()
}

// ---- which message becomes which record -----------------------------------------------------------------------------------------

fn session_script(rig: &mut Rig) {
    rig.agentd(request(1, "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})))
        .engine(response(1, json!({"protocolVersion": 1, "agentInfo": {"name": "e", "version": "1"}, "agentCapabilities": {"loadSession": true}})))
        .agentd(request(2, "session/new", json!({"cwd": "/mnt/workspace/r", "mcpServers": []})))
        .engine(response(2, json!({"sessionId": SID, "modes": {"currentModeId": "act", "availableModes": []}})))
        .agentd(prompt(3, "Add a percent_change function\n  (typed as is, \u{e9} \u{1F600})\n"))
        .engine(chunk("I will "))
        .engine(chunk("start now."))
        .engine(tool_call("t1", "ls", json!({"command": "ls"})))
        .engine(tool_done("t1", json!("README.md\nsrc\n")))
        .engine(response(3, json!({"stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}})));
}

#[test]
fn a_session_becomes_the_records_of_the_decision_in_order_numbered_from_zero() {
    let mut rig = Rig::new(Settings::default());
    session_script(&mut rig);
    rig.finish();
    let kinds = rig.kinds();
    assert_eq!(
        kinds,
        [
            "opaque",
            "opaque",
            "opaque",
            "session",
            "prompt",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
            "prompt_result"
        ]
    );
    let recs = rig.records();
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(r["n"], i as u64, "numbered from 0 without a gap");
        assert!(
            r["t"]
                .as_str()
                .is_some_and(|t| t.ends_with('Z') && t.len() == 20),
            "{}",
            r["t"]
        );
        assert!(r["data"].is_object(), "{r}");
    }
    let dirs: Vec<&str> = recs.iter().map(|r| r["dir"].as_str().unwrap()).collect();
    assert_eq!(
        dirs,
        ["out", "in", "out", "in", "out", "in", "in", "in", "in"]
    );
    // the session record: the session/new result and the initialize result, once
    assert_eq!(recs[3]["data"]["sessionId"], SID);
    assert_eq!(recs[3]["data"]["result"]["modes"]["currentModeId"], "act");
    assert_eq!(recs[3]["data"]["initialize"]["agentInfo"]["name"], "e");
    // the prompt is stored byte for byte
    assert_eq!(
        recs[4]["data"]["prompt"][0]["text"],
        "Add a percent_change function\n  (typed as is, \u{e9} \u{1F600})\n"
    );
    assert_eq!(recs[4]["data"]["sessionId"], SID);
    // the message is one record with the joined text
    assert_eq!(recs[5]["data"]["content"]["text"], "I will start now.");
    // the tool records keep ACP's own members, `kind` included
    assert_eq!(recs[6]["data"]["kind"], "execute");
    assert_eq!(recs[6]["data"]["toolCallId"], "t1");
    assert_eq!(recs[7]["data"]["rawOutput"], "README.md\nsrc\n");
    // the reply of the prompt carries the stop reason and the usage, which no update does
    assert_eq!(recs[8]["data"]["result"]["stopReason"], "end_turn");
    assert_eq!(recs[8]["data"]["result"]["usage"]["totalTokens"], 15);
    assert_eq!(recs[8]["data"]["id"], 3);
}

#[test]
fn a_failed_prompt_is_a_record_too_and_a_cancel_is_one() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(7, "go"))
        .agentd(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": SID}}))
        .engine(json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32603, "message": "The operation timed out.", "data": {"isRetryable": true}}}))
        .finish();
    assert_eq!(rig.kinds(), ["prompt", "cancel", "prompt_result"]);
    let recs = rig.records();
    assert_eq!(recs[1]["dir"], "out");
    assert_eq!(recs[1]["data"]["sessionId"], SID);
    assert_eq!(recs[2]["data"]["error"]["code"], -32603);
    assert_eq!(recs[2]["data"]["id"], 7);
}

#[test]
fn a_permission_request_and_the_answer_are_records() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(request(900, "session/request_permission", json!({"sessionId": SID, "toolCall": {"toolCallId": "t9", "title": "rm"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]})))
        .agentd(response(900, json!({"outcome": {"outcome": "selected", "optionId": "allow"}})))
        .finish();
    assert_eq!(rig.kinds(), ["permission_request", "permission_response"]);
    let recs = rig.records();
    assert_eq!(
        (recs[0]["dir"].as_str(), recs[1]["dir"].as_str()),
        (Some("in"), Some("out"))
    );
    assert_eq!(recs[0]["data"]["id"], 900);
    assert_eq!(recs[0]["data"]["params"]["toolCall"]["title"], "rm");
    assert_eq!(recs[1]["data"]["id"], 900);
    assert_eq!(recs[1]["data"]["result"]["outcome"]["optionId"], "allow");
}

#[test]
fn the_other_update_kinds_are_stored_with_their_own_words() {
    let mut rig = Rig::new(Settings::default());
    for u in [
        json!({"sessionUpdate": "plan", "entries": [{"content": "x", "priority": "high", "status": "pending"}]}),
        json!({"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "init", "description": "d"}]}),
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
        json!({"sessionUpdate": "session_info_update", "title": "t"}),
        json!({"sessionUpdate": "usage_update", "used": 5, "size": 100}),
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hello"}}),
    ] {
        rig.engine(update(u));
    }
    rig.finish();
    assert_eq!(
        rig.kinds(),
        [
            "plan",
            "available_commands_update",
            "current_mode_update",
            "session_info_update",
            "usage_update",
            "user_message_chunk"
        ]
    );
}

#[test]
fn an_unknown_update_or_method_is_an_opaque_record_with_its_name_its_size_and_a_cut_payload() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(update(
        json!({"sessionUpdate": "future_thing", "blob": "z".repeat(20_000)}),
    ))
    .agentd(request(
        40,
        "session/set_mode",
        json!({"sessionId": SID, "modeId": "plan"}),
    ))
    .engine(json!({"jsonrpc": "2.0", "method": "x/unknown", "params": {"a": 1}}))
    .finish();
    assert_eq!(rig.kinds(), ["opaque", "opaque", "opaque"]);
    let recs = rig.records();
    assert_eq!(recs[0]["data"]["variant"], "future_thing");
    assert!(recs[0]["data"]["bytes"].as_u64().unwrap() > 20_000);
    let payload = serde_json::to_string(&recs[0]["data"]["payload"]).unwrap();
    assert!(
        payload.len() <= 4096 + 64,
        "the payload is cut at 4 KiB: {}",
        payload.len()
    );
    assert_eq!(recs[1]["data"]["method"], "session/set_mode");
    assert_eq!(recs[2]["data"]["method"], "x/unknown");
}

#[test]
fn a_member_that_is_not_a_short_word_is_not_trusted_as_a_kind() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(update(json!({"sessionUpdate": "x".repeat(500)})))
        .engine(update(
            json!({"sessionUpdate": "has spaces and (odd) characters"}),
        ))
        .engine(json!({"jsonrpc": "2.0", "method": "session/update", "params": "not an object"}))
        .finish();
    assert_eq!(rig.kinds(), ["opaque", "opaque", "opaque"]);
}

// ---- what is never stored ---------------------------------------------------------------------------------------------------------

#[test]
fn thought_text_is_stored_nowhere() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(thought("THOUGHT-ONE the model reasons about the plan"))
        .engine(chunk("answer"))
        .engine(thought("THOUGHT-TWO"))
        .finish();
    assert_eq!(rig.kinds(), ["agent_message_chunk"]);
    let all = rig.all_bytes();
    assert!(!all.contains("THOUGHT"), "{all}");
    assert_eq!(rig.p.stats().excluded, 2);
}

#[test]
fn history_the_engine_replays_while_a_session_is_loaded_is_left_out() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(request(5, "session/load", json!({"sessionId": SID, "cwd": "/w", "mcpServers": []})))
        .engine(update(json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "OLD USER"}})))
        .engine(chunk("OLD ANSWER"))
        .engine(tool_call("old", "ls", json!({})))
        .engine(response(5, json!({"configOptions": []})))
        .engine(chunk("NEW ANSWER"))
        .finish();
    let all = rig.all_bytes();
    assert!(!all.contains("OLD"), "{all}");
    assert!(all.contains("NEW ANSWER"));
    assert_eq!(rig.kinds(), ["opaque", "session", "agent_message_chunk"]);
    assert_eq!(rig.p.stats().excluded, 3);
}

#[test]
fn a_message_of_an_abandoned_session_is_not_stored() {
    let mut rig = Rig::new(Settings::default());
    rig.stale(chunk("LATE NOTE FROM THE OLD SESSION"))
        .engine(chunk("fresh"))
        .finish();
    assert_eq!(rig.message_text(), "fresh");
    assert!(!rig.all_bytes().contains("LATE"));
}

#[test]
fn a_config_option_update_is_stored_only_when_it_changes() {
    let mut rig = Rig::new(Settings::default());
    let big = json!({"sessionUpdate": "config_option_update", "configOptions": [{"id": "model", "currentValue": "m1", "options": (0..50).map(|i| json!({"value": format!("m{i}"), "name": format!("Model {i}")})).collect::<Vec<_>>()}]});
    let other = json!({"sessionUpdate": "config_option_update", "configOptions": [{"id": "model", "currentValue": "m2"}]});
    rig.engine(update(big.clone()))
        .engine(update(big))
        .engine(update(other.clone()))
        .engine(update(other))
        .finish();
    assert_eq!(
        rig.kinds(),
        ["config_option_update", "config_option_update"]
    );
}

#[test]
fn binary_looking_tool_output_is_replaced_by_its_size() {
    let mut rig = Rig::new(Settings::default());
    let binary: String = (0..4000)
        .map(|i| {
            if i % 3 == 0 {
                'a'
            } else {
                char::from_u32(1 + (i % 8) as u32).unwrap()
            }
        })
        .collect();
    rig.engine(tool_call(
        "b1",
        "cat data.bin",
        json!({"command": "cat data.bin"}),
    ))
    .engine(tool_done("b1", json!(binary)))
    .engine(tool_call("b2", "echo", json!({})))
    .engine(tool_done(
        "b2",
        json!("plain text with a tab\tand lines\nof output\n"),
    ))
    .finish();
    let recs = rig.records();
    let out = recs[1]["data"]["rawOutput"].as_str().unwrap();
    assert!(out.starts_with("[binary output withheld"), "{out}");
    assert!(out.contains("4000"), "the size is named: {out}");
    assert_eq!(
        recs[3]["data"]["rawOutput"],
        "plain text with a tab\tand lines\nof output\n"
    );
    assert_eq!(rig.p.stats().excluded, 1);
}

// ---- credential files -------------------------------------------------------------------------------------------------------------

#[test]
fn the_output_of_a_tool_that_names_a_credential_file_is_withheld() {
    for (title, input) in [
        ("cat .env", json!({"command": "cat .env"})),
        ("read file", json!({"path": "/repo/.env.production"})),
        ("read file", json!({"file_path": "deploy/server.pem"})),
        (
            "cat ~/.ssh/id_ed25519",
            json!({"command": "cat ~/.ssh/id_ed25519"}),
        ),
        ("read", json!({"path": "/home/u/.npmrc"})),
        ("read", json!({"path": "/home/u/.netrc"})),
        ("read", json!({"path": "/home/u/.aws/credentials"})),
        ("cat '.env'", json!({})),
    ] {
        let mut rig = Rig::new(Settings::default());
        rig.engine(tool_call("c1", title, input.clone()))
            .engine(tool_done(
                "c1",
                json!("NOT-A-PATTERN-BUT-PRIVATE=value-in-the-file"),
            ))
            .finish();
        let all = rig.all_bytes();
        assert!(!all.contains("PRIVATE=value"), "{title} {input}: {all}");
        assert!(all.contains("[file contents withheld]"), "{title} {input}");
    }
}

#[test]
fn a_location_names_the_file_too() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(update(json!({"sessionUpdate": "tool_call", "toolCallId": "l1", "title": "Read", "kind": "read", "locations": [{"path": "/w/.env"}]})))
        .engine(update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "l1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "SECRETLESS-BUT-PRIVATE"}}]})))
        .finish();
    let all = rig.all_bytes();
    assert!(!all.contains("PRIVATE"), "{all}");
    assert!(all.contains("[file contents withheld]"));
}

#[test]
fn ordinary_files_and_look_alikes_are_stored() {
    for (title, input) in [
        ("read src/main.rs", json!({"path": "src/main.rs"})),
        ("read id_utils.rs", json!({"path": "src/id_utils.rs"})),
        (
            "read environment.md",
            json!({"path": "docs/environment.md"}),
        ),
        (
            "read credentials_test.go",
            json!({"path": "pkg/credentials_test.go"}),
        ),
        ("cat pem_parser.py", json!({"command": "cat pem_parser.py"})),
        ("grep env", json!({"command": "grep -r ENV_VAR ."})),
    ] {
        let mut rig = Rig::new(Settings::default());
        rig.engine(tool_call("o1", title, input))
            .engine(tool_done("o1", json!("an ordinary output")))
            .finish();
        assert!(rig.all_bytes().contains("an ordinary output"), "{title}");
    }
}

// ---- joined text and redaction -----------------------------------------------------------------------------------------------------

#[test]
fn chunks_of_one_message_are_one_record_and_another_message_ends_it() {
    let mut rig = Rig::new(Settings::default());
    for i in 0..1000 {
        rig.engine(chunk(&format!("{:04}", i % 10000)));
    }
    assert!(
        rig.lines.is_empty(),
        "nothing is written while the message is short and fresh"
    );
    rig.engine(tool_call("t", "ls", json!({}))).finish();
    assert_eq!(rig.kinds(), ["agent_message_chunk", "tool_call"]);
    let want: String = (0..1000).map(|i| format!("{:04}", i % 10000)).collect();
    assert_eq!(rig.message_text(), want);
}

#[test]
fn a_long_message_is_written_in_fragments_that_join_to_the_whole_text() {
    let mut rig = Rig::new(small());
    let text = filler(20_000);
    for piece in text.as_bytes().chunks(4) {
        rig.engine(chunk(std::str::from_utf8(piece).unwrap()));
    }
    let before_end = rig.lines.len();
    assert!(
        before_end >= 8,
        "the message is flushed as it grows: {before_end} fragments before its end"
    );
    rig.finish();
    assert_eq!(rig.message_text(), text);
    let recs = rig.records();
    assert!(recs.iter().all(|r| r["kind"] == "agent_message_chunk"));
    let ids: std::collections::BTreeSet<&str> = recs
        .iter()
        .map(|r| r["data"]["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "the fragments of one message share one id: {ids:?}"
    );
}

#[test]
fn the_timer_writes_what_has_waited_two_seconds_and_names_its_deadline() {
    let mut rig = Rig::new(Settings {
        window: 256,
        max_hold: 4096,
        ..Settings::default()
    });
    assert_eq!(rig.p.deadline(), None, "nothing pending, no deadline");
    rig.at(0.0).engine(chunk(&filler(2_000)));
    assert!(rig.lines.is_empty(), "under the size trigger");
    assert_eq!(rig.p.deadline(), Some(rig.t0 + secs(2.0)));
    rig.at(1.9).tick();
    assert!(rig.lines.is_empty());
    rig.at(2.1).tick();
    assert_eq!(
        rig.kinds(),
        ["agent_message_chunk"],
        "the text before the held window is written"
    );
    let written = rig.message_text().len();
    assert!(
        written > 1_500 && written < 2_000,
        "{written}: all but the window"
    );
    assert!(
        rig.p.deadline().is_some_and(|d| d >= rig.t0 + secs(4.0)),
        "the next deadline is two seconds on"
    );
    rig.finish();
    assert_eq!(rig.message_text(), filler(2_000));
}

#[test]
fn a_message_with_its_own_id_is_kept_under_it_and_a_new_id_starts_a_new_message() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(chunk_of("m-1", "one "))
        .engine(chunk_of("m-1", "two"))
        .engine(chunk_of("m-2", "three"))
        .finish();
    let recs = rig.records();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["data"]["messageId"], "m-1");
    assert_eq!(recs[0]["data"]["content"]["text"], "one two");
    assert_eq!(recs[1]["data"]["messageId"], "m-2");
}

#[test]
fn a_message_without_an_id_gets_one_and_the_next_message_another() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(chunk("first"))
        .engine(tool_call("t", "ls", json!({})))
        .engine(chunk("second"))
        .finish();
    let recs = rig.records();
    let a = recs[0]["data"]["messageId"].as_str().unwrap();
    let b = recs[2]["data"]["messageId"].as_str().unwrap();
    assert!(!a.is_empty() && !b.is_empty() && a != b, "{a} {b}");
}

#[test]
fn a_chunk_that_is_not_text_ends_the_message_and_is_stored_as_it_is() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(chunk("before"))
        .engine(update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}})))
        .engine(chunk("after"))
        .finish();
    assert_eq!(
        rig.kinds(),
        [
            "agent_message_chunk",
            "agent_message_chunk",
            "agent_message_chunk"
        ]
    );
    let recs = rig.records();
    assert_eq!(recs[0]["data"]["content"]["text"], "before");
    assert_eq!(recs[1]["data"]["content"]["type"], "image");
    assert_eq!(recs[2]["data"]["content"]["text"], "after");
}

#[test]
fn a_secret_split_across_the_chunks_of_a_message_never_reaches_a_record() {
    let token = github_token(3);
    let text = format!(
        "{} export GH={token} and then {}",
        filler(1500),
        filler(1500)
    );
    for piece in [1usize, 2, 3, 5, 9, 64] {
        let mut rig = Rig::new(small());
        for c in text.as_bytes().chunks(piece) {
            rig.engine(chunk(std::str::from_utf8(c).unwrap()));
            let all = rig.all_bytes();
            for part in token.as_bytes().windows(12) {
                assert!(
                    !all.contains(std::str::from_utf8(part).unwrap()),
                    "pieces of {piece}: {part:?} was written"
                );
            }
        }
        rig.finish();
        let all = rig.all_bytes();
        assert!(!all.contains(&token), "pieces of {piece}");
        assert!(
            rig.message_text().contains("export GH=<redacted> and then"),
            "pieces of {piece}"
        );
    }
}

#[test]
fn a_secret_in_a_tool_output_or_input_is_redacted_before_the_record_exists() {
    let token = github_token(7);
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call("t1", "run", json!({"command": format!("git clone https://x:{token}@host/r.git"), "env": {"GITHUB_TOKEN": token.clone()}})))
        .engine(tool_done("t1", json!({"stdout": format!("Authorization: Bearer {token}\nok"), "nested": [{"deep": token.clone()}]})))
        .finish();
    let all = rig.all_bytes();
    assert!(!all.contains(&token), "{all}");
    assert!(!all.contains(&token[..20]));
    assert!(all.contains("<redacted>"));
    assert!(all.contains("ok"), "the rest of the output stays");
}

#[test]
fn a_secret_in_a_property_name_is_redacted_too() {
    let token = github_token(11);
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_done("t1", json!({ token.clone(): "value" })))
        .finish();
    assert!(!rig.all_bytes().contains(&token));
}

#[test]
fn the_session_secrets_go_from_the_prompt_the_output_and_the_replies() {
    let secret = "wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY0123456789".to_string();
    let mut rig = Rig::with(Settings::default(), Redactor::new(vec![secret.clone()]));
    rig.agentd(prompt(1, &format!("use {secret} please")))
        .engine(tool_done("t", json!(format!("echo {secret}"))))
        .engine(response(
            1,
            json!({"stopReason": "end_turn", "note": secret.clone()}),
        ))
        .finish();
    assert!(!rig.all_bytes().contains(&secret));
    assert!(rig.all_bytes().contains("use <redacted> please"));
}

// ---- caps ------------------------------------------------------------------------------------------------------------------------------

fn random_text(n: usize) -> String {
    // not compressible by a trick of the test, not secret-shaped
    let mut x: u64 = 0x9E3779B97F4A7C15;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (b'a' + (x % 26) as u8) as char
        })
        .collect()
}

#[test]
fn a_record_over_the_cap_keeps_its_shape_its_head_and_its_tail_and_says_what_it_cut() {
    let cap = Settings::default().record_cap;
    assert_eq!(cap, 256 * 1024);
    let output = format!("HEAD-MARK {} TAIL-MARK", random_text(400_000));
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_done("t1", json!(output.clone()))).finish();
    let line = &rig.lines[0];
    assert!(line.bytes.len() <= cap + 512, "{} bytes", line.bytes.len());
    let rec = &rig.records()[0];
    assert_eq!(rec["kind"], "tool_call_update");
    assert_eq!(rec["data"]["toolCallId"], "t1");
    assert_eq!(rec["data"]["status"], "completed");
    let text = rec["data"]["rawOutput"].as_str().unwrap();
    assert!(text.starts_with("HEAD-MARK"));
    assert!(text.ends_with("TAIL-MARK"));
    assert!(text.contains("cut"), "the cut is marked in the text");
    let t = &rec["data"]["_truncated"];
    assert!(t["originalBytes"].as_u64().unwrap() > 400_000);
    let hex = t["sha256"].as_str().unwrap();
    assert_eq!(hex.len(), 64);
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(rig.p.stats().truncated, 1);
}

#[test]
fn the_digest_of_a_cut_record_is_the_digest_of_what_was_cut() {
    let output = random_text(300_000);
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_done("t1", json!(output.clone()))).finish();
    let rec = &rig.records()[0];
    // the digest covers the redacted record before the cut: the record as it would have been stored
    let original = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "rawOutput": output});
    let want: String = Sha256::digest(serde_json::to_vec(&original).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(rec["data"]["_truncated"]["sha256"], want);
    assert_eq!(
        rec["data"]["_truncated"]["originalBytes"],
        serde_json::to_vec(&original).unwrap().len() as u64
    );
}

#[test]
fn a_record_at_the_cap_is_not_cut_and_one_byte_over_is() {
    let s = Settings {
        record_cap: 4096,
        ..Settings::default()
    };
    let base = serde_json::to_vec(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "rawOutput": ""})).unwrap().len();
    let mut rig = Rig::new(s.clone());
    rig.engine(tool_done("t1", json!("x".repeat(4096 - base))))
        .finish();
    assert!(rig.records()[0]["data"].get("_truncated").is_none());
    let mut rig = Rig::new(s);
    rig.engine(tool_done("t1", json!("x".repeat(4096 - base + 1))))
        .finish();
    assert!(rig.records()[0]["data"].get("_truncated").is_some());
}

#[test]
fn a_huge_tool_input_is_cut_like_an_output() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call(
        "t1",
        "write",
        json!({"path": "big.txt", "content": random_text(300_000)}),
    ))
    .finish();
    let line = &rig.lines[0];
    assert!(line.bytes.len() <= 256 * 1024 + 512);
    let rec = &rig.records()[0];
    assert_eq!(rec["data"]["rawInput"]["path"], "big.txt");
    assert!(rec["data"]["_truncated"].is_object());
}

/// The deepest node of `v`, a scalar being depth 0 at the root: the reader's rule (`ham_core::transcript::json_depth`) refuses a whole
/// chunk when one record's payload is deeper than 32.
fn depth(v: &Value) -> usize {
    let (mut deepest, mut stack) = (0, vec![(v, 0usize)]);
    while let Some((value, d)) = stack.pop() {
        deepest = deepest.max(d);
        match value {
            Value::Array(a) => stack.extend(a.iter().map(|i| (i, d + 1))),
            Value::Object(m) => stack.extend(m.values().map(|i| (i, d + 1))),
            _ => {}
        }
    }
    deepest
}

/// `levels` objects inside one another, with a string at the bottom.
fn nested(levels: usize, leaf: &str) -> Value {
    let mut v = json!(leaf);
    for _ in 0..levels {
        v = json!({ "k": v });
    }
    v
}

#[test]
fn a_tool_input_nested_deeper_than_the_readers_limit_is_cut_so_the_whole_chunk_stays_readable() {
    // serde_json accepts 128 levels from the engine; the reader refuses the whole chunk past 32 in one record
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call("t1", "run", nested(100, "bottom")))
        .finish();
    let rec = &rig.records()[0];
    assert_eq!(rec["kind"], "tool_call");
    assert_eq!(
        rec["data"]["toolCallId"], "t1",
        "the rest of the record is kept"
    );
    assert!(depth(&rec["data"]) <= 32, "depth {}", depth(&rec["data"]));
    assert!(
        rig.all_bytes().contains("nested too deeply"),
        "the cut is marked in the text"
    );
    assert_eq!(rig.p.stats().truncated, 1);
}

#[test]
fn a_tool_output_nested_deeper_than_the_readers_limit_is_cut_too() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_done("t1", nested(120, "bottom"))).finish();
    let rec = &rig.records()[0];
    assert_eq!(rec["data"]["status"], "completed");
    assert!(depth(&rec["data"]) <= 32, "depth {}", depth(&rec["data"]));
}

#[test]
fn the_deepest_nesting_the_reader_accepts_is_kept_whole_and_one_level_more_is_cut() {
    // data is an object holding rawInput, so the leaf of `nested(n)` is n + 1 levels below data; a scalar at depth 32 is allowed
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call("t1", "run", nested(31, "bottom")))
        .finish();
    let rec = &rig.records()[0];
    assert_eq!(depth(&rec["data"]), 32);
    assert!(!rig.all_bytes().contains("nested too deeply"));
    assert_eq!(rig.p.stats().truncated, 0);
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call("t1", "run", nested(32, "bottom")))
        .finish();
    assert!(depth(&rig.records()[0]["data"]) <= 32);
    assert!(rig.all_bytes().contains("nested too deeply"));
    assert_eq!(rig.p.stats().truncated, 1);
}

#[test]
fn a_record_that_is_mostly_structure_is_replaced_by_its_digest() {
    let many: Vec<Value> = (0..40_000).map(|i| json!({"k": i})).collect();
    let mut rig = Rig::new(Settings::default());
    rig.engine(update(json!({"sessionUpdate": "plan", "entries": many})))
        .finish();
    let line = &rig.lines[0];
    assert!(line.bytes.len() <= 128 * 1024 + 512, "{}", line.bytes.len());
    let rec = &rig.records()[0];
    assert_eq!(rec["kind"], "plan");
    assert!(rec["data"]["_truncated"]["originalBytes"].as_u64().unwrap() > 128 * 1024);
}

#[test]
fn the_huge_record_is_cut_before_redaction_so_the_rules_see_a_bounded_text() {
    // 6 MB in one string: the pre-cap keeps the work bounded and the secret in the tail still goes
    let token = github_token(5);
    let big = format!(
        "{}{} {token} end",
        "y".repeat(3_000_000),
        "z".repeat(3_000_000)
    );
    let mut rig = Rig::new(Settings::default());
    let start = Instant::now();
    rig.engine(tool_done("t1", json!(big))).finish();
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(!rig.all_bytes().contains(&token));
    assert!(
        rig.lines[0].bytes.len() <= 256 * 1024 + 512,
        "{} bytes",
        rig.lines[0].bytes.len()
    );
}

// ---- numbering and gaps ----------------------------------------------------------------------------------------------------------

#[test]
fn a_gap_record_for_shed_messages_takes_the_next_number() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(tool_call("t", "ls", json!({})));
    let out = rig.p.note_shed(&rig.r, 7, "queue", 1_790_000_001);
    rig.lines.extend(out);
    rig.engine(tool_done("t", json!("ok"))).finish();
    assert_eq!(rig.kinds(), ["tool_call", "gap", "tool_call_update"]);
    let recs = rig.records();
    assert_eq!(recs[1]["data"]["kind"], "shed");
    assert_eq!(recs[1]["data"]["count"], 7);
    assert_eq!(recs[1]["data"]["reason"], "queue");
    assert_eq!(
        (
            recs[0]["n"].as_u64(),
            recs[1]["n"].as_u64(),
            recs[2]["n"].as_u64()
        ),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!(rig.p.stats().shed, 7);
}

#[test]
fn a_message_in_progress_is_written_before_a_gap_so_the_order_is_the_order_it_happened_in() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(chunk("some words"));
    let out = rig.p.note_shed(&rig.r, 1, "queue", 1_790_000_001);
    rig.lines.extend(out);
    rig.finish();
    assert_eq!(rig.kinds(), ["agent_message_chunk", "gap"]);
}

#[test]
fn the_same_input_gives_the_same_bytes() {
    let run = || {
        let mut rig = Rig::new(Settings::default());
        session_script(&mut rig);
        rig.finish();
        rig.lines
            .iter()
            .map(|l| l.bytes.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn finish_writes_what_the_window_still_holds_and_pending_text_is_never_lost() {
    let mut rig = Rig::new(small());
    rig.engine(chunk(
        "a short message that is entirely inside the held window",
    ))
    .finish();
    assert_eq!(
        rig.message_text(),
        "a short message that is entirely inside the held window"
    );
}

#[test]
fn every_line_is_one_json_object_without_a_newline() {
    let mut rig = Rig::new(Settings::default());
    session_script(&mut rig);
    rig.engine(tool_done("t2", json!("line one\nline two\r\nline three")))
        .finish();
    for l in &rig.lines {
        assert!(!l.bytes.contains(&b'\n') && !l.bytes.contains(&b'\r'));
        assert!(
            serde_json::from_slice::<Value>(&l.bytes)
                .unwrap()
                .is_object()
        );
    }
}

// ---- which prompt a record belongs to -------------------------------------------------------------------------

/// The `turn_id` member of a record: the id of the open `session/prompt`, `null` outside one, and `"MISSING"` when the member is absent.
fn turn_of(rec: &Value) -> Value {
    rec.get("turn_id").cloned().unwrap_or(json!("MISSING"))
}

fn other_session_chunk(sid: &str, text: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
}

fn plain_update(word: &str) -> Value {
    update(json!({ "sessionUpdate": word }))
}

#[test]
fn every_record_inside_an_open_prompt_carries_the_id_of_that_prompt() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(3, "go"))
        .engine(tool_call("t1", "run", json!({"command": "ls"})))
        .engine(chunk("hello"))
        .engine(response(3, json!({"stopReason": "end_turn"})))
        .finish();
    let recs = rig.records();
    assert_eq!(
        rig.kinds(),
        [
            "prompt",
            "tool_call",
            "agent_message_chunk",
            "prompt_result"
        ]
    );
    for r in &recs {
        assert_eq!(turn_of(r), json!(3), "{r}");
    }
    assert_eq!(rig.p.stats().unattributed, 0);
}

#[test]
fn an_update_outside_an_open_prompt_gets_turn_id_null_and_is_counted() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(plain_update("available_commands_update")) // before any prompt
        .agentd(prompt(3, "go"))
        .engine(chunk("working"))
        .engine(response(3, json!({"stopReason": "end_turn"})))
        .engine(plain_update("usage_update")) // after the answer
        .engine(chunk("a late word"))
        .finish();
    let got: Vec<(String, Value)> = rig
        .records()
        .iter()
        .map(|r| (r["kind"].as_str().unwrap().to_string(), turn_of(r)))
        .collect();
    assert_eq!(
        got,
        [
            ("available_commands_update".to_string(), json!(null)),
            ("prompt".to_string(), json!(3)),
            ("agent_message_chunk".to_string(), json!(3)),
            ("prompt_result".to_string(), json!(3)),
            ("usage_update".to_string(), json!(null)),
            ("agent_message_chunk".to_string(), json!(null)),
        ]
    );
    assert_eq!(
        rig.p.stats().unattributed,
        3,
        "the three updates outside a prompt"
    );
}

#[test]
fn a_message_in_progress_keeps_the_prompt_it_started_in() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(3, "go"))
        .engine(chunk("first part "))
        .engine(response(3, json!({"stopReason": "end_turn"})))
        .engine(chunk("after the answer"))
        .finish();
    let texts: Vec<(String, Value)> = rig
        .records()
        .iter()
        .filter(|r| r["kind"] == "agent_message_chunk")
        .map(|r| {
            (
                r["data"]["content"]["text"].as_str().unwrap().to_string(),
                turn_of(r),
            )
        })
        .collect();
    assert_eq!(
        texts,
        [
            ("first part ".to_string(), json!(3)),
            ("after the answer".to_string(), json!(null))
        ]
    );
    assert_eq!(rig.p.stats().unattributed, 1);
}

#[test]
fn a_prompt_adopts_only_the_updates_of_its_own_session() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(3, "go"))
        .engine(other_session_chunk("sess-2", "from another session"))
        .engine(chunk("from this one"))
        .finish();
    let turns: Vec<Value> = rig
        .records()
        .iter()
        .filter(|r| r["kind"] == "agent_message_chunk")
        .map(turn_of)
        .collect();
    assert_eq!(turns, [json!(null), json!(3)]);
    assert_eq!(rig.p.stats().unattributed, 1);
}

#[test]
fn a_new_engine_process_closes_every_open_prompt() {
    // the old process died with prompt 3 unanswered: what the new one says before its own prompt is not that prompt's
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(3, "go"))
        .agentd(request(1, "initialize", json!({"protocolVersion": 1})))
        .engine(chunk("hello again"))
        .finish();
    let last = rig.records().pop().unwrap();
    assert_eq!(last["kind"], "agent_message_chunk");
    assert_eq!(turn_of(&last), json!(null));
    assert_eq!(rig.p.stats().unattributed, 1);
}

#[test]
fn a_prompt_id_may_be_a_string_and_is_kept_as_it_came() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(
        json!({"jsonrpc": "2.0", "id": "p-9", "method": "session/prompt",
        "params": {"sessionId": SID, "prompt": [{"type": "text", "text": "go"}]}}),
    )
    .engine(chunk("hello"))
    .finish();
    for r in rig.records() {
        assert_eq!(turn_of(&r), json!("p-9"), "{r}");
    }
}

#[test]
fn history_the_engine_replays_while_a_session_loads_is_not_counted() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(request(
        9,
        "session/load",
        json!({"sessionId": SID, "cwd": "/w"}),
    ))
    .engine(chunk("an old answer"))
    .engine(response(9, json!({})))
    .finish();
    assert_eq!(rig.p.stats().excluded, 1);
    assert_eq!(rig.p.stats().unattributed, 0, "history is not live traffic");
}

#[test]
fn a_thought_outside_a_prompt_is_counted_though_it_is_stored_nowhere() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(thought("hmm")).finish();
    assert!(rig.lines.is_empty());
    assert_eq!(rig.p.stats().unattributed, 1);
}

#[test]
fn the_end_record_carries_the_count_and_agentds_own_records_carry_no_turn_id() {
    let mut rig = Rig::new(Settings::default());
    rig.engine(plain_update("usage_update"))
        .engine(plain_update("plan"))
        .finish();
    let end = rig.p.end(1_790_000_000);
    let rec: Value = serde_json::from_slice(&end.bytes).unwrap();
    assert_eq!(rec["kind"], "end");
    assert_eq!(rec["data"], json!({"reason": "final", "unattributed": 2}));
    assert!(
        rec.get("turn_id").is_none(),
        "agentd wrote it, no prompt owns it"
    );
    let gap = rig.p.gap(1_790_000_000, json!({"kind": "failed"}));
    let g: Value = serde_json::from_slice(&gap.bytes).unwrap();
    assert!(g.get("turn_id").is_none());
}

#[test]
fn a_run_with_nothing_outside_a_prompt_ends_exactly_as_before() {
    let mut rig = Rig::new(Settings::default());
    rig.agentd(prompt(3, "go"))
        .engine(response(3, json!({"stopReason": "end_turn"})))
        .finish();
    let end = rig.p.end(1_790_000_000);
    let rec: Value = serde_json::from_slice(&end.bytes).unwrap();
    assert_eq!(rec["data"], json!({"reason": "final"}));
}
