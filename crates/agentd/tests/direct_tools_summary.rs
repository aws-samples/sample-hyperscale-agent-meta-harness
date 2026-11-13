//! The tool hook's note in the run summary and what it logs.
//!
//! `summary.json` gets `tools {enabled, port, connect, list[], calls, failed, refused, truncated, restarts, p50Ms, p95Ms}`: counts and words, and
//! nothing a user or a model typed. A log line holds a tool name, a result word, a duration and a byte count. The tests plant a canary in
//! the query, in the answer and in the registration's neighbours, and look for it everywhere the module writes.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod hook_support;
mod tools_support;

use agentd::direct::tools::ToolsHook;
use agentd::direct::tools::settings::Settings;
use hook_support::{Rig, prompt_sent, session_started};
use serde_json::json;
use tools_support::echo::Echo;
use tools_support::{notification, request, rpc, search};

const CANARY: &str = "CANARY-QUERY-5c1e";

fn any_port() -> Settings {
    Settings {
        port: 0,
        ..Settings::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn counts_only() {
    let mut rig = Rig::new().await;
    let (echo, state) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    session_started(&hook, "s1", 1);
    prompt_sent(&hook, "s1", 1);
    let addr = rig.addr();
    let init = request(
        json!(0),
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    rpc(addr, &init).await;
    rpc(addr, &notification("notifications/initialized", json!({}))).await;
    rpc(addr, &request(json!(1), "tools/list", json!({}))).await;
    // A call that answers with the canary, one that fails with it in the input, one whose input is wrong, and a plain one.
    let a = rpc(addr, &search(json!(2), &format!("echo:{CANARY}")))
        .await
        .json();
    assert!(
        a["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(CANARY)
    );
    rpc(
        addr,
        &search(json!(3), &format!("fail:unavailable:{CANARY}")),
    )
    .await;
    rpc(
        addr,
        &request(
            json!(4),
            "tools/call",
            json!({"name": "code_search", "arguments": {"wrong": CANARY}}),
        ),
    )
    .await;
    rpc(addr, &search(json!(5), "echo:plain")).await;
    assert!(state.entered.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    hook.stop_for_tests().await;

    let note = hook.note().expect("the module ran");
    assert!(note.is_valid(), "{note:?}");
    assert!(note.enabled && note.reason.is_none());
    assert_eq!(note.port, hook.port());
    assert_eq!(note.list, ["code_search"]);
    let counts = note.counts.clone().expect("counts: the server ran");
    // Four `tools/call` requests for a listed tool, however they ended.
    assert_eq!(counts.calls.get("code_search"), Some(&4), "{counts:?}");
    assert_eq!(
        counts.refused, 1,
        "the call whose input was wrong did not run"
    );
    assert_eq!(counts.restarts, 0);
    assert!(counts.p50_ms.is_some() && counts.p95_ms.is_some());

    // The canary is in no word the module wrote: not in the note, not in a log line of the run.
    let note_text = serde_json::to_string(&note).unwrap();
    assert!(!note_text.contains(CANARY), "{note_text}");
    let lines = rig.d.log.lines().join("\n");
    assert!(
        !lines.contains(CANARY),
        "a log line holds the canary:\n{lines}"
    );
    assert!(!lines.contains("echo:"), "{lines}");
    assert_eq!(counts.failed, 1, "the call whose backend failed");
    // The note survives the wire (the whole-run test validates it inside a summary).
    let again: harness_protocol::direct::DirectToolsNote =
        serde_json::from_str(&note_text).unwrap();
    assert_eq!(again, note);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_says_what_happened_in_words_and_numbers() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let port = hook.port().unwrap();
    let lines = rig.tools_log();
    // One line for the server, with the port and the number of tools; one for the registration, with a byte count.
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("127.0.0.1:{port}")) && l.contains("1 tool")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("registration written") && l.contains("bytes")),
        "{lines:?}"
    );
    // No path of the run's directories, no URL with a path, no rendered file text.
    let all = lines.join("\n");
    assert!(!all.contains(rig.data_dir.to_str().unwrap()), "{all}");
    assert!(!all.contains("mcpServers"), "{all}");
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_stopped_for_good_says_why() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    // The serving task ends four times: the module restarted it three times and gives up.
    for n in 1..=4u32 {
        hook.crash_server_for_tests();
        let wanted = n.min(3);
        let reached = tools_support::until(std::time::Duration::from_secs(5), || {
            hook.stats().unwrap().restarts >= wanted
        })
        .await;
        assert!(reached.is_some(), "restart {n}");
        if n < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    hook.stop_for_tests().await;
    let note = hook.note().unwrap();
    assert!(note.enabled, "the module started it");
    assert_eq!(note.reason.as_deref(), Some("restarts-used"), "{note:?}");
    assert_eq!(note.counts.as_ref().unwrap().restarts, 3);
    assert!(note.is_valid());
}
