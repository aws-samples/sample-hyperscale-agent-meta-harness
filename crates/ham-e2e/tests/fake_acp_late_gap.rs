//! `FAKE_ACP_LATE_GAP_MS=<n>` of the fake engine (FLK1, 2026-10-17): after the response to a prompt, each further message of the same reply is
//! written `n` milliseconds after the one before it. A real engine writes its late updates (a usage figure, a word of text) when it has them, and a
//! loaded host spreads them out; the knob makes that spread deterministic so a test can prove an ordering that does not depend on the
//! speed of the machine. Without the variable the engine writes the whole reply at once, as before.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCENARIO: &str = "updates_outside_a_prompt";
const GAP_MS: u64 = 300;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh workspace; the name is unique, so nothing is ever deleted.
fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = base.join(format!("fake-acp-late-{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(d.join("ws")).unwrap();
    d.join("ws")
}

/// How many messages the first turn of the scenario sends after the response to its prompt.
fn late_messages_of_turn_1() -> usize {
    let text = std::fs::read_to_string(
        repo()
            .join("fixtures/engines/fake-acp/transcripts/direct")
            .join(SCENARIO)
            .join("turn-1.jsonl"),
    )
    .unwrap();
    let agent: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|l| l["direction"] == "agent")
        .map(|l| l["message"].clone())
        .collect();
    let response = agent
        .iter()
        .position(|m| m.get("id").is_some() && m.get("method").is_none())
        .expect("the turn has a response");
    agent.len() - response - 1
}

/// One reply of the engine to the first prompt: every message with the instant it was read, and the instant just before the prompt was sent.
struct Reply {
    sent_at: Instant,
    /// `(read at, the message)`, in the order they came.
    messages: Vec<(Instant, Value)>,
    /// The position of the prompt's response in `messages`.
    response: usize,
}

fn first_reply(gap_ms: Option<u64>) -> Reply {
    let ws = scratch("reply");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"));
    cmd.arg("--transcripts")
        .arg(repo().join("fixtures/engines/fake-acp/transcripts"))
        .arg("--workspace")
        .arg(&ws)
        .env("FAKE_ACP_SCENARIO", SCENARIO)
        .env_remove("FAKE_ACP_LATE_GAP_MS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(ms) = gap_ms {
        cmd.env("FAKE_ACP_LATE_GAP_MS", ms.to_string());
    }
    let mut child = cmd.spawn().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let mut send = |id: u64, method: &str, params: Value| {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        stdin.flush().unwrap();
    };
    let read_response_to = |out: &mut BufReader<std::process::ChildStdout>, id: u64| loop {
        let mut line = String::new();
        assert!(out.read_line(&mut line).unwrap() > 0, "the engine closed");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
            return v;
        }
    };
    send(1, "initialize", json!({"protocolVersion": 1}));
    read_response_to(&mut out, 1);
    send(2, "session/new", json!({"cwd": ws, "mcpServers": []}));
    let sid = read_response_to(&mut out, 2)["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let sent_at = Instant::now();
    send(
        3,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}),
    );
    let late = late_messages_of_turn_1();
    let mut messages = Vec::new();
    let mut response = None;
    while response.is_none() || messages.len() < response.unwrap() + 1 + late {
        let mut line = String::new();
        assert!(out.read_line(&mut line).unwrap() > 0, "the engine closed");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("id") == Some(&json!(3)) && v.get("method").is_none() {
            response = Some(messages.len());
        }
        messages.push((Instant::now(), v));
    }
    drop(stdin);
    let _ = child.wait();
    Reply {
        sent_at,
        messages,
        response: response.unwrap(),
    }
}

#[test]
fn with_a_gap_each_message_after_the_prompt_response_comes_a_gap_after_the_one_before_it() {
    let late = late_messages_of_turn_1();
    assert!(late >= 2, "the scenario sends late updates: {late}");
    let r = first_reply(Some(GAP_MS));
    assert_eq!(r.messages.len(), r.response + 1 + late);
    // The response was written after the prompt was sent, and the engine waits the gap before each further message, so message k after the
    // response cannot have been read earlier than k gaps after the prompt was sent: a bound that no scheduling delay can break.
    for k in 1..=late {
        let at = r.messages[r.response + k].0;
        assert!(
            at.duration_since(r.sent_at) >= Duration::from_millis(GAP_MS * k as u64),
            "late message {k} was read {:?} after the prompt; the gap is {GAP_MS} ms",
            at.duration_since(r.sent_at)
        );
    }
}

#[test]
fn the_gap_changes_when_the_messages_come_and_not_which_messages_or_their_order() {
    let plain = first_reply(None);
    let gapped = first_reply(Some(50));
    let content = |r: &Reply| -> Vec<Value> { r.messages.iter().map(|m| m.1.clone()).collect() };
    assert_eq!(content(&plain), content(&gapped));
    assert_eq!(plain.response, gapped.response);
}
