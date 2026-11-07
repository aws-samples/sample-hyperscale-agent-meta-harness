//! The tool server of agentd: stateless MCP over streamable HTTP on the loopback of the microVM.
//! Everything here goes through a socket with a client that starts no process: the answers are read the way the engine's own client reads them
//! (status, headers and a JSON body), and the backend is the echo backend of `examples/tools_server_support/echo.rs`.
//!
//! The package's nine cases come first (their names are the package's), then the cases that came out of the review of the design: how a call
//! ends (a cancel, an abort after the grace, a hang-up, an unexpected end of the server, the backstop), the strict reading of Host and Origin, the
//! envelope rules, and what the log and the counters hold.
#![allow(clippy::unwrap_used)]

mod common;
mod tools_support;

use agentd::direct::tools::server::{
    self, Limits, MAX_BODY_BYTES, MAX_INFLIGHT, SUPPORTED_VERSIONS, ServerState, bind,
    is_loopback_host, is_loopback_origin, is_loopback_peer,
};
use harness_protocol::tools::{DirectToolsNote, SERVER_NAME, TOOLS, tools_list_result};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tools_support::*;

fn initialize(id: i64, version: &str) -> Value {
    request(
        json!(id),
        "initialize",
        json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    )
}

// ---- the package's cases ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn initialize_negotiates_and_never_issues_a_session_id() {
    let rig = Rig::start().await;
    for (asked, want) in [
        ("2025-11-25", "2025-11-25"),
        ("2025-06-18", "2025-06-18"),
        ("2025-03-26", "2025-03-26"),
        ("2099-01-01", "2025-11-25"),
        ("2024-11-05", "2025-11-25"),
    ] {
        let r = rpc(rig.addr, &initialize(1, asked)).await;
        assert_eq!(r.status, 200, "{asked}");
        assert_eq!(r.header("content-type"), Some("application/json"));
        let v = r.json();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], want, "asked for {asked}");
        assert_eq!(v["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(
            v["result"]["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
    }
    // `tools_support::raw` fails any answer that carries `Mcp-Session-Id`; a client that sends one anyway is served all the same.
    let r = rpc_with(
        rig.addr,
        &[("Mcp-Session-Id", "left-over-from-a-server-that-restarted")],
        &request(json!(2), "tools/list", json!({})),
    )
    .await;
    assert_eq!(r.status, 200);
    // The three versions are the table of the module, highest first.
    assert_eq!(
        SUPPORTED_VERSIONS,
        ["2025-11-25", "2025-06-18", "2025-03-26"]
    );
    // A missing or mistyped version is a bad parameter, an answer to this request.
    for params in [json!({}), json!({"protocolVersion": 20251125})] {
        let r = rpc(rig.addr, &request(json!(3), "initialize", params)).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["error"]["code"], -32602);
        assert_eq!(r.json()["id"], 3);
    }
    rig.stop().await;
}

#[tokio::test]
async fn get_and_delete_are_405_and_bad_host_or_origin_is_403() {
    let rig = Rig::start().await;
    let port = rig.addr.port();
    for method in ["GET", "DELETE", "PUT", "PATCH"] {
        let r = raw(rig.addr, method, "/mcp", &[], b"").await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("POST"), "{method}");
    }
    let ping = request(json!(1), "ping", json!({}));
    // A Host that is not a loopback name, or an Origin that is not a loopback origin, is refused before anything is read.
    let r = rpc_with(rig.addr, &[("Host", "example.com")], &ping).await;
    assert_eq!(r.status, 403);
    let r = rpc_with(rig.addr, &[("Origin", "http://example.com")], &ping).await;
    assert_eq!(r.status, 403);
    // The check comes first, also for a method that would have been a 405.
    let r = raw(rig.addr, "GET", "/mcp", &[("Host", "example.com")], b"").await;
    assert_eq!(r.status, 403);
    // The refusal does not repeat what was sent.
    assert!(!r.body.contains("example.com"));
    // Loopback names pass.
    for host in [
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        "localhost".to_string(),
    ] {
        let r = rpc_with(rig.addr, &[("Host", host.as_str())], &ping).await;
        assert_eq!(r.status, 200, "{host}");
    }
    for origin in [
        format!("http://localhost:{port}"),
        "http://127.0.0.1:3000".to_string(),
        "https://localhost".to_string(),
        "http://[::1]:8080".to_string(),
    ] {
        let r = rpc_with(rig.addr, &[("Origin", origin.as_str())], &ping).await;
        assert_eq!(r.status, 200, "{origin}");
    }
    // Another path is not the endpoint.
    for path in ["/", "/mcp/", "/sse", "/healthz"] {
        let r = raw(rig.addr, "POST", path, &[], ping.to_string().as_bytes()).await;
        assert_eq!(r.status, 404, "{path}");
    }
    rig.stop().await;
}

#[tokio::test]
async fn bodies_batches_and_unknown_methods() {
    let rig = Rig::start().await;
    // At the limit, a body is served; one byte over, it is 413 whatever it holds.
    let pad = |total: usize| {
        let base =
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"pad": ""}}).to_string();
        let fill = total - base.len();
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"pad": "a".repeat(fill)}})
            .to_string()
    };
    // The numbers are the package's, pinned here so that a change of the constant is a change of this test.
    assert_eq!(MAX_BODY_BYTES, 64 * 1024);
    let at_limit = pad(MAX_BODY_BYTES);
    assert_eq!(at_limit.len(), MAX_BODY_BYTES);
    let r = raw(rig.addr, "POST", "/mcp", &[], at_limit.as_bytes()).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["result"], json!({}));
    let over = pad(MAX_BODY_BYTES + 1);
    let r = raw(rig.addr, "POST", "/mcp", &[], over.as_bytes()).await;
    assert_eq!(r.status, 413);
    let big = pad(MAX_BODY_BYTES + 5_000);
    let r = raw(rig.addr, "POST", "/mcp", &[], big.as_bytes()).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["code"], -32600);

    // A batch is not supported (a request the server cannot attribute to one id).
    let batch = json!([
        request(json!(1), "ping", json!({})),
        request(json!(2), "ping", json!({}))
    ]);
    let r = rpc(rig.addr, &batch).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32600);
    assert_eq!(r.json()["id"], Value::Null);
    // An unknown method of an identified request is an answer to it.
    let r = rpc(
        rig.addr,
        &request(json!("abc"), "resources/list", json!({})),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["error"]["code"], -32601);
    assert_eq!(r.json()["id"], "abc");
    // `notifications/initialized` is accepted and answered with nothing.
    let r = rpc(
        rig.addr,
        &notification("notifications/initialized", json!({})),
    )
    .await;
    assert_eq!(r.status, 202);
    assert_eq!(r.body, "");
    // A notification of any other name is accepted too; it is never an error to send one.
    let r = rpc(
        rig.addr,
        &notification("notifications/roots/list_changed", json!({})),
    )
    .await;
    assert_eq!(r.status, 202);
    assert_eq!(r.body, "");
    // `ping` is an empty object.
    let r = rpc(rig.addr, &request(json!(7), "ping", json!({}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"jsonrpc": "2.0", "id": 7, "result": {}}));
    // Text that is not JSON.
    let r = raw(rig.addr, "POST", "/mcp", &[], b"{not json").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], -32700);
    assert_eq!(r.json()["id"], Value::Null);
    assert!(!r.body.contains("not json"), "the answer repeats the body");
    rig.stop().await;
}

#[tokio::test]
async fn tools_list_holds_only_the_backends_given() {
    let empty = Rig::empty().await;
    let r = rpc(empty.addr, &request(json!(1), "tools/list", json!({}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["result"], json!({"tools": []}));
    assert!(empty.shared.listed().is_empty());
    empty.stop().await;

    let rig = Rig::start().await;
    let r = rpc(rig.addr, &request(json!(1), "tools/list", json!({}))).await;
    let result = r.json()["result"].clone();
    let tools = result["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    // The name, the description and the input schema are the table's, word for word.
    assert_eq!(tools[0]["name"], "code_search");
    assert_eq!(tools[0]["description"], TOOLS[0].description);
    assert_eq!(tools[0]["inputSchema"], (TOOLS[0].input_schema)());
    // With every table tool served, the answer is the table's own result byte for byte.
    assert_eq!(result, tools_list_result());
    assert_eq!(rig.shared.listed(), vec!["code_search"]);
    // A cursor is ignored: the list is one page.
    let r = rpc(
        rig.addr,
        &request(json!(2), "tools/list", json!({"cursor": "x"})),
    )
    .await;
    assert_eq!(r.json()["result"], tools_list_result());
    rig.stop().await;
}

#[tokio::test]
async fn tools_call_validates_input_and_puts_failures_first() {
    let rig = Rig::start().await;
    let wrong = rpc(
        rig.addr,
        &request(
            json!(1),
            "tools/call",
            json!({"name": "code_search", "arguments": {"wrong-member-xyz": 1}}),
        ),
    )
    .await;
    assert_eq!(wrong.status, 200);
    let v = wrong.json();
    assert_eq!(v["result"]["isError"], true);
    let text = text_of(&v);
    assert!(text.starts_with("error: usage: "), "{text}");
    assert!(
        !text.contains("wrong-member-xyz"),
        "the text repeats a member name: {text}"
    );
    assert!(v["result"].get("structuredContent").is_none());
    // The backend never saw the call.
    assert_eq!(
        rig.echo.entered.load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    // No pattern, a pattern of the wrong type, no arguments at all, arguments that are not an object.
    for args in [json!({}), json!({"pattern": 5}), Value::Null, json!([1])] {
        let call = json!({"name": "code_search", "arguments": args});
        let r = rpc(rig.addr, &request(json!(2), "tools/call", call)).await;
        let text = text_of(&r.json());
        assert!(text.starts_with("error: usage: "), "{args}: {text}");
        assert_eq!(r.json()["result"]["isError"], true);
    }
    let r = rpc(
        rig.addr,
        &request(json!(3), "tools/call", json!({"name": "code_search"})),
    )
    .await;
    assert!(
        text_of(&r.json()).starts_with("error: usage: pattern"),
        "{}",
        r.body
    );

    // A name the server does not list is a fault of the protocol; the name is not repeated.
    let r = rpc(
        rig.addr,
        &request(
            json!(4),
            "tools/call",
            json!({"name": "no-such-tool-qq", "arguments": {}}),
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["error"]["code"], -32602);
    assert_eq!(r.json()["id"], 4);
    assert!(!r.body.contains("no-such-tool-qq"));
    // The name the engine shows (with the server's prefix) is not a name of the table either.
    let r = rpc(
        rig.addr,
        &request(
            json!(5),
            "tools/call",
            json!({"name": "ham__code_search", "arguments": {"pattern": "x"}}),
        ),
    )
    .await;
    assert_eq!(r.json()["error"]["code"], -32602);
    // A call without a name.
    let r = rpc(
        rig.addr,
        &request(json!(6), "tools/call", json!({"arguments": {}})),
    )
    .await;
    assert_eq!(r.json()["error"]["code"], -32602);
    let r = rpc(rig.addr, &request(json!(7), "tools/call", json!("x"))).await;
    assert_eq!(r.json()["error"]["code"], -32602);

    // A good call: the answer is text, with `isError` false and nothing else besides the content.
    let r = rpc(rig.addr, &search(json!(8), "hello")).await;
    let v = r.json();
    assert_eq!(text_of(&v), "echo: hello");
    assert_eq!(v["result"]["isError"], false);
    let mut keys: Vec<&String> = v["result"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["content", "isError"]);
    assert_eq!(v["result"]["content"].as_array().unwrap().len(), 1);
    assert_eq!(v["result"]["content"][0]["type"], "text");

    // A backend's own failure: the word first, the detail after it, `isError` true.
    for (pattern, word) in [
        ("fail:timeout", "timeout"),
        ("fail:unavailable", "unavailable"),
        ("fail:refused", "refused"),
        ("fail:disabled", "disabled"),
        ("fail:usage", "usage"),
        ("fail:busy", "busy"),
    ] {
        let r = rpc(rig.addr, &search(json!(9), pattern)).await;
        let v = r.json();
        assert_eq!(v["result"]["isError"], true, "{pattern}");
        assert_eq!(text_of(&v), format!("error: {word}: test"), "{pattern}");
    }
    rig.stop().await;
}

#[tokio::test]
async fn the_ninth_call_in_flight_is_told_busy() {
    assert_eq!(MAX_INFLIGHT, 8);
    let rig = Rig::start().await;
    let addr = rig.addr;
    let mut calls = Vec::new();
    for i in 0..MAX_INFLIGHT {
        calls.push(tokio::spawn(async move {
            rpc(addr, &search(json!(100 + i), "gate")).await.json()
        }));
    }
    let entered = until(Duration::from_secs(10), || {
        rig.echo.entered.load(std::sync::atomic::Ordering::SeqCst) == MAX_INFLIGHT
    })
    .await;
    assert!(
        entered.is_some(),
        "eight calls did not get into the backend"
    );
    // The ninth gets an answer at once, and the backend does not see it.
    let t = std::time::Instant::now();
    let r = rpc(addr, &search(json!(200), "echo:ninth")).await.json();
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(r["result"]["isError"], true);
    assert!(text_of(&r).starts_with("error: busy"), "{r}");
    assert_eq!(
        rig.echo.entered.load(std::sync::atomic::Ordering::SeqCst),
        MAX_INFLIGHT
    );
    // Calls that are not tool calls are not held back by it.
    let r = rpc(addr, &request(json!(201), "ping", json!({}))).await;
    assert_eq!(r.status, 200);
    rig.echo.release();
    for c in calls {
        let v = c.await.unwrap();
        assert_eq!(text_of(&v), "released");
    }
    // The slots are free again.
    let r = rpc(addr, &search(json!(202), "echo:again")).await.json();
    assert_eq!(text_of(&r), "echo: again");
    let stats = rig.shared.stats();
    assert_eq!(stats.refused, 1);
    assert_eq!(stats.calls["code_search"], MAX_INFLIGHT as u64 + 2);
    rig.stop().await;
}

/// The serialized `result` of an answer, in UTF-16 code units: what a JavaScript client's `JSON.stringify(result).length` counts.
fn units(answer: &Value) -> usize {
    serde_json::to_string(&answer["result"])
        .unwrap()
        .encode_utf16()
        .count()
}

#[tokio::test]
async fn results_are_content_only_and_fit_the_cap() {
    let rig = Rig::start().await;
    // A 100,000-character answer.
    let v = rpc(rig.addr, &search(json!(1), "big:100000")).await.json();
    assert!(units(&v) <= 7_500, "{} units", units(&v));
    assert!(v["result"].get("structuredContent").is_none());
    assert_eq!(v["result"]["isError"], false);
    let text = text_of(&v);
    assert!(text.contains("shortened"), "{}", &text[text.len() - 80..]);
    assert!(text.starts_with("aaaa"));
    // An answer that fits is not touched.
    let v = rpc(rig.addr, &search(json!(2), "echo:short")).await.json();
    assert_eq!(text_of(&v), "echo: short");
    // The cap is on the serialized form: quotes and backslashes double, control characters grow to six, an astral character is two units.
    for unit in [
        "\"",
        "\\",
        "\u{1}",
        "\u{1F600}",
        "line one\nline two\n",
        "\u{e9}\u{4e2d}",
    ] {
        let v = rpc(rig.addr, &search(json!(3), &format!("rep:20000:{unit}")))
            .await
            .json();
        assert!(units(&v) <= 7_500, "{unit:?}: {} units", units(&v));
        assert!(text_of(&v).contains("shortened"), "{unit:?}");
    }
    // Lines are cut at a line end when one lies close to the end of what fits: no half of a match.
    let v = rpc(rig.addr, &search(json!(4), "lines:5000")).await.json();
    let text = text_of(&v);
    let body = text.split("\n[shortened").next().unwrap();
    assert!(
        body.ends_with(';'),
        "{:?}",
        &body[body.len().saturating_sub(40)..]
    );
    let all: String = (0..5000)
        .map(|i| format!("src/file.rs:{i}: let x = {i};\n"))
        .collect();
    let total = all.chars().count();
    assert!(
        text.contains(&format!(" of {total} characters]")),
        "the notice names the total: {}",
        &text[text.len() - 70..]
    );
    // The shortened answers are counted; a backend's own marker is counted too and not repeated by the server.
    let before = rig.shared.stats().truncated;
    let v = rpc(rig.addr, &search(json!(5), "trunc")).await.json();
    assert_eq!(text_of(&v), "partial");
    assert_eq!(rig.shared.stats().truncated, before + 1);
    rig.stop().await;

    // A cap below the floor is raised to it (a profile value of 10 would leave no room for a notice).
    let small = Rig::with(10, Limits::default(), true).await;
    let v = rpc(small.addr, &search(json!(1), "big:5000")).await.json();
    assert!(units(&v) <= 256 && units(&v) > 100, "{}", units(&v));
    small.stop().await;
    // A larger cap is honoured (another engine may take more).
    let large = Rig::with(16_000, Limits::default(), true).await;
    let v = rpc(large.addr, &search(json!(1), "big:12000")).await.json();
    assert_eq!(text_of(&v).len(), 12_000);
    large.stop().await;
}

#[tokio::test]
async fn cancel_stops_the_call_and_kills_its_child() {
    let rig = Rig::start().await;
    let addr = rig.addr;
    let call = tokio::spawn(async move {
        rpc_with(
            addr,
            &[("User-Agent", "ua-a")],
            &search(json!(77), "sleep:30"),
        )
        .await
    });
    assert!(
        until(Duration::from_secs(10), || !rig.echo.pids().is_empty())
            .await
            .is_some()
    );
    let pid = rig.echo.pids()[0];
    assert!(!gone(pid), "the child is not running");
    let t = std::time::Instant::now();
    let r = rpc_with(
        addr,
        &[("User-Agent", "ua-a")],
        &notification(
            "notifications/cancelled",
            json!({"requestId": 77, "reason": "user"}),
        ),
    )
    .await;
    assert_eq!(r.status, 202);
    let answer = call.await.unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "the call ended after {:?}",
        t.elapsed()
    );
    // The cancelled request gets an answer for the connection's sake: the cancel code, not a tool result.
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["error"]["code"], -32800);
    assert_eq!(answer.json()["id"], 77);
    assert!(
        until(Duration::from_secs(1), || gone(pid)).await.is_some(),
        "the child {pid} is still there"
    );
    // Cancelled calls are counted as received and as nothing else.
    let s = rig.shared.stats();
    assert_eq!(
        (s.calls["code_search"], s.failed, s.refused, s.cancelled),
        (1, 0, 0, 1)
    );
    // The slot is free and the server serves the next call.
    let r = rpc(addr, &search(json!(78), "echo:next")).await.json();
    assert_eq!(text_of(&r), "echo: next");
    // A cancel for a call that is not in flight does nothing, and does not cancel a later call that gets the same number.
    let r = rpc_with(
        addr,
        &[("User-Agent", "ua-a")],
        &notification("notifications/cancelled", json!({"requestId": 79})),
    )
    .await;
    assert_eq!(r.status, 202);
    let r = rpc_with(
        addr,
        &[("User-Agent", "ua-a")],
        &search(json!(79), "echo:late"),
    )
    .await
    .json();
    assert_eq!(text_of(&r), "echo: late");
    rig.stop().await;
}

#[tokio::test]
async fn a_cancel_is_matched_on_the_client_program_and_the_request_number() {
    let rig = Rig::start().await;
    let addr = rig.addr;
    let a = tokio::spawn(async move {
        rpc_with(
            addr,
            &[("User-Agent", "ua-a")],
            &search(json!(5), "sleep:30"),
        )
        .await
    });
    let b = tokio::spawn(async move {
        rpc_with(
            addr,
            &[("User-Agent", "ua-b")],
            &search(json!(5), "sleep:30"),
        )
        .await
    });
    assert!(
        until(Duration::from_secs(10), || rig.echo.pids().len() == 2)
            .await
            .is_some()
    );
    let pids = rig.echo.pids();
    // Both clients count from the same number; a cancel from A ends A's call and leaves B's alone.
    rpc_with(
        addr,
        &[("User-Agent", "ua-a")],
        &notification("notifications/cancelled", json!({"requestId": 5})),
    )
    .await;
    let answer = tokio::time::timeout(Duration::from_secs(2), a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.json()["error"]["code"], -32800);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!b.is_finished(), "the other client's call ended with it");
    let alive: Vec<bool> = pids.iter().map(|p| !gone(*p)).collect();
    assert_eq!(alive.iter().filter(|x| **x).count(), 1, "{alive:?}");
    rpc_with(
        addr,
        &[("User-Agent", "ua-b")],
        &notification("notifications/cancelled", json!({"requestId": 5})),
    )
    .await;
    let answer = tokio::time::timeout(Duration::from_secs(2), b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.json()["error"]["code"], -32800);
    assert!(
        until(Duration::from_secs(1), || pids.iter().all(|p| gone(*p)))
            .await
            .is_some()
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_call_that_ignores_the_cancel_is_aborted_after_the_grace_and_its_child_dies() {
    let limits = Limits {
        cancel_grace: Duration::from_millis(300),
        ..Limits::default()
    };
    let rig = Rig::with(7_500, limits, true).await;
    let addr = rig.addr;
    let call = tokio::spawn(async move { rpc(addr, &search(json!(1), "stubborn:30")).await });
    assert!(
        until(Duration::from_secs(10), || !rig.echo.pids().is_empty())
            .await
            .is_some()
    );
    let pid = rig.echo.pids()[0];
    let t = std::time::Instant::now();
    rpc(
        addr,
        &notification("notifications/cancelled", json!({"requestId": 1})),
    )
    .await;
    let answer = call.await.unwrap();
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(250),
        "it did not wait for the backend: {took:?}"
    );
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!(answer.json()["error"]["code"], -32800);
    assert!(
        until(Duration::from_secs(1), || gone(pid)).await.is_some(),
        "the child {pid} outlived the abort"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_client_that_hangs_up_ends_its_call() {
    // `sleep` obeys the cancel signal that the hang-up fires; `stubborn` ignores it, so only the abort of the task (which drops the backend's
    // future and with it the guard of the child) can end it.
    for pattern in ["sleep:30", "stubborn:30"] {
        let rig = Rig::start().await;
        let mut s = tokio::net::TcpStream::connect(rig.addr).await.unwrap();
        let body = search(json!(1), pattern).to_string();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            rig.addr,
            body.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body.as_bytes()).await.unwrap();
        assert!(
            until(Duration::from_secs(10), || !rig.echo.pids().is_empty())
                .await
                .is_some()
        );
        let pid = rig.echo.pids()[0];
        drop(s);
        assert!(
            until(Duration::from_secs(3), || gone(pid)).await.is_some(),
            "{pattern}: the child {pid} outlived the connection of its call"
        );
        // The slot is back.
        let r = rpc(rig.addr, &search(json!(2), "echo:after")).await.json();
        assert_eq!(text_of(&r), "echo: after");
        rig.stop().await;
    }
}

#[tokio::test]
async fn a_backend_that_panics_answers_unavailable_and_the_server_goes_on() {
    let rig = Rig::start().await;
    let v = rpc(rig.addr, &search(json!(1), "panic")).await.json();
    assert_eq!(v["result"]["isError"], true);
    let text = text_of(&v);
    assert!(text.starts_with("error: unavailable"), "{text}");
    assert!(
        text.contains("do not retry"),
        "a panic may have left effects: {text}"
    );
    assert!(
        !text.contains("told to panic"),
        "the panic text is not passed on: {text}"
    );
    let v = rpc(rig.addr, &search(json!(2), "echo:fine")).await.json();
    assert_eq!(text_of(&v), "echo: fine");
    let s = rig.shared.stats();
    assert_eq!((s.failed, s.refused), (1, 0));
    assert!(!rig.log_text().contains("told to panic"));
    rig.stop().await;
}

#[tokio::test]
async fn a_hung_backend_ends_at_the_backstop_and_frees_its_slot() {
    let limits = Limits {
        call_backstop: Duration::from_millis(400),
        ..Limits::default()
    };
    let rig = Rig::with(7_500, limits, true).await;
    let t = std::time::Instant::now();
    let v = rpc(rig.addr, &search(json!(1), "hang")).await.json();
    assert!(
        t.elapsed() >= Duration::from_millis(350) && t.elapsed() < Duration::from_secs(3),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(v["result"]["isError"], true);
    assert!(text_of(&v).starts_with("error: timeout"), "{}", text_of(&v));
    assert_eq!(rig.shared.stats().failed, 1);
    let v = rpc(rig.addr, &search(json!(2), "echo:next")).await.json();
    assert_eq!(text_of(&v), "echo: next");
    rig.stop().await;
}

#[tokio::test]
async fn a_slow_body_gets_a_408_and_the_connection_is_released() {
    let limits = Limits {
        body_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let rig = Rig::with(7_500, limits, true).await;
    let mut s = tokio::net::TcpStream::connect(rig.addr).await.unwrap();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"jsonrpc\"",
        rig.addr
    );
    s.write_all(head.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
    assert!(read.is_ok(), "the server waited for a body that never came");
    assert!(
        String::from_utf8_lossy(&out).starts_with("HTTP/1.1 408"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    rig.stop().await;
}

// ---- restart ------------------------------------------------------------------------------------------------------------------------

async fn wait_restarts(rig: &Rig, n: u32) {
    let ok = until(Duration::from_secs(10), || rig.handle().restarts() == n).await;
    assert!(
        ok.is_some(),
        "restarts stayed at {} (wanted {n})",
        rig.handle().restarts()
    );
}

#[tokio::test]
async fn the_server_restarts_on_the_same_port_and_a_client_does_not_notice() {
    let rig = Rig::start().await;
    let addr = rig.handle().addr();
    // A client does its handshake once.
    let r = rpc(addr, &initialize(1, "2025-11-25")).await;
    assert_eq!(r.status, 200);
    rpc(addr, &notification("notifications/initialized", json!({}))).await;
    let before = text_of(&rpc(addr, &search(json!(2), "echo:one")).await.json());
    assert_eq!(before, "echo: one");

    // The task ends on purpose, three times: each time it is up again on the same port, and the next call of a client that did not
    // initialize again is served (the server keeps no state of a client).
    for n in 1..=3 {
        if n % 2 == 1 {
            rig.handle().crash_for_tests();
        } else {
            rig.handle().restart_gracefully_for_tests();
        }
        wait_restarts(&rig, n).await;
        assert_eq!(rig.handle().addr(), addr);
        assert_eq!(rig.handle().state(), ServerState::Up);
        let r = rpc(addr, &search(json!(10 + n), "echo:after")).await;
        assert_eq!(text_of(&r.json()), "echo: after", "after restart {n}");
        assert_eq!(r.json()["id"], 10 + n);
    }
    assert_eq!(rig.shared.stats().restarts, 3);
    assert!(rig.log_text().contains("restarted"));

    // The fourth end leaves it down: the port refuses, the state says why, and the log says it once.
    rig.handle().crash_for_tests();
    let down = until(Duration::from_secs(10), || {
        matches!(rig.handle().state(), ServerState::Down(_))
    })
    .await;
    assert!(down.is_some(), "the server was started a fourth time");
    assert_eq!(rig.handle().state(), ServerState::Down("restarts-used"));
    assert!(refuses(addr).await, "the port still answers");
    let said = rig
        .log
        .lines()
        .iter()
        .filter(|l| l.contains("stopped for good"))
        .count();
    assert_eq!(said, 1, "{}", rig.log_text());
    rig.stop().await;
}

#[tokio::test]
async fn an_unexpected_end_cancels_the_calls_in_flight_and_the_server_comes_back() {
    let rig = Rig::start().await;
    let addr = rig.addr;
    let call = tokio::spawn(async move { rpc(addr, &search(json!(1), "sleep:30")).await });
    assert!(
        until(Duration::from_secs(10), || !rig.echo.pids().is_empty())
            .await
            .is_some()
    );
    let pid = rig.echo.pids()[0];
    rig.handle().crash_for_tests();
    // The call is ended and its child is gone: a call is never replayed and never outlives the task that served it.
    let answer = tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.json()["error"]["code"], -32800);
    assert!(
        until(Duration::from_secs(1), || gone(pid)).await.is_some(),
        "the child {pid} survived the restart"
    );
    wait_restarts(&rig, 1).await;
    let r = rpc(addr, &search(json!(2), "echo:again")).await.json();
    assert_eq!(text_of(&r), "echo: again");
    rig.stop().await;
}

#[tokio::test]
async fn a_stopped_server_refuses_connections() {
    let rig = Rig::start().await;
    let addr = rig.addr;
    assert_eq!(
        rpc(addr, &request(json!(1), "ping", json!({})))
            .await
            .status,
        200
    );
    let (shared, log) = (rig.shared.clone(), rig.log.clone());
    rig.stop().await;
    assert!(refuses(addr).await, "the port still answers after the stop");
    // A stop is not an unexpected end of the serving task: nothing is started again.
    assert_eq!(shared.stats().restarts, 0);
    assert!(
        !log.lines().iter().any(|l| l.contains("restarted")),
        "{:?}",
        log.lines()
    );
}

// ---- the loopback ----------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_listener_that_is_not_on_the_loopback_is_closed_and_serves_nothing() {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Rig::empty().await.shared;
    let handle = server::start(listener, shared);
    assert_eq!(handle.state(), ServerState::Down("not-loopback"));
    let to = SocketAddr::from(([127, 0, 0, 1], port));
    assert!(refuses(to).await, "the listener is still open");
    handle.stop().await;
}

#[tokio::test]
async fn bind_takes_the_loopback_and_falls_back_to_a_free_port_when_the_port_is_taken() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let l = bind(port).await.unwrap();
    let got = l.local_addr().unwrap();
    assert!(got.ip().is_loopback());
    assert_ne!(got.port(), port);
    let any = bind(0).await.unwrap();
    assert!(any.local_addr().unwrap().ip().is_loopback());
}

#[test]
fn only_a_loopback_peer_is_served() {
    let peer = |s: &str| s.parse::<SocketAddr>().unwrap();
    for ok in [
        "127.0.0.1:5000",
        "127.9.9.9:5000",
        "[::1]:5000",
        "[::ffff:127.0.0.1]:5000",
    ] {
        assert!(is_loopback_peer(&peer(ok)), "{ok}");
    }
    for no in [
        "10.0.0.1:5000",
        "0.0.0.0:5000",
        "[2001:db8::1]:5000",
        "[::ffff:10.0.0.1]:5000",
        "192.168.1.1:80",
    ] {
        assert!(!is_loopback_peer(&peer(no)), "{no}");
    }
}

#[test]
fn host_and_origin_are_read_strictly() {
    for host in [
        "localhost",
        "localhost:3130",
        "LocalHost:3130",
        "127.0.0.1",
        "127.0.0.1:3130",
        "127.0.0.2:80",
        "[::1]",
        "[::1]:3130",
        "[::ffff:127.0.0.1]:80",
    ] {
        assert!(is_loopback_host(host), "{host}");
    }
    for host in [
        "",
        " ",
        "example.com",
        "localhost.example.com",
        "127.0.0.1.example.com",
        "example.com:127.0.0.1",
        "localhost@example.com",
        "user@127.0.0.1",
        "127.0.0.1@example.com",
        "localhost:",
        "localhost:99999",
        "localhost:80:80",
        "localhost:http",
        "127.0.0.1:-1",
        "0.0.0.0",
        "0.0.0.0:3130",
        "10.0.0.1",
        "127.1",
        "2130706433",
        "::1",
        "[::1",
        "::1]",
        "[::1]x",
        "[::1]:",
        "[localhost]",
        "[127.0.0.1]",
        "localhost/",
        "localhost#127.0.0.1",
        "localhost?x",
        "local host",
        "localhost\u{0}",
        " localhost",
        "localhost ",
        "xn--localhost",
        "[::2]",
        "[fe80::1]:80",
        "localhost\\evil.com",
        "localhost,evil.com",
    ] {
        assert!(!is_loopback_host(host), "{host:?}");
    }
    for origin in [
        "http://localhost",
        "http://localhost:3000",
        "https://localhost:8443",
        "http://127.0.0.1",
        "http://127.0.0.1:3130",
        "http://[::1]",
        "http://[::1]:3130",
    ] {
        assert!(is_loopback_origin(origin), "{origin}");
    }
    for origin in [
        "",
        "null",
        "localhost",
        "127.0.0.1:3130",
        "http://example.com",
        "http://localhost.example.com",
        "http://example.com#@127.0.0.1",
        "http://127.0.0.1@example.com",
        "http://localhost/",
        "http://localhost/path",
        "http://localhost?x=1",
        "ftp://localhost",
        "file://localhost",
        "vscode-webview://localhost",
        "http://",
        "https://",
        "http:///",
        "http://localhost:",
        "http://localhost:99999",
        "HTTP://localhost:80x",
        "http://0.0.0.0",
        "http://localhost\\evil.com",
        "http://local host",
        " http://localhost",
        "http://localhost ",
    ] {
        assert!(!is_loopback_origin(origin), "{origin:?}");
    }
}

// ---- the envelope -------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn messages_that_are_not_a_request_get_the_status_the_rules_name() {
    let rig = Rig::start().await;
    // Not usable as a request: 400, an error with no id.
    for (label, msg) in [
        (
            "null id",
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
        ),
        (
            "object id",
            json!({"jsonrpc": "2.0", "id": {"a": 1}, "method": "ping"}),
        ),
        (
            "array id",
            json!({"jsonrpc": "2.0", "id": [1], "method": "ping"}),
        ),
        (
            "float id",
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
        ),
        (
            "bool id",
            json!({"jsonrpc": "2.0", "id": true, "method": "ping"}),
        ),
        ("no jsonrpc member", json!({"id": 1, "method": "ping"})),
        (
            "old jsonrpc",
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        ),
        (
            "method not text",
            json!({"jsonrpc": "2.0", "id": 1, "method": 5}),
        ),
        ("id and nothing else", json!({"jsonrpc": "2.0", "id": 1})),
        ("not an object", json!("ping")),
        ("a number", json!(7)),
        ("empty batch", json!([])),
        ("empty object", json!({})),
    ] {
        let r = rpc(rig.addr, &msg).await;
        assert_eq!(r.status, 400, "{label}: {}", r.body);
        let v = r.json();
        assert_eq!(v["error"]["code"], -32600, "{label}");
        assert_eq!(v["id"], Value::Null, "{label}");
    }
    // Accepted and answered with nothing: a notification (no id) of any name, and a response of the client.
    for (label, msg) in [
        (
            "notification",
            notification("notifications/whatever", json!({"x": 1})),
        ),
        (
            "notification without params",
            json!({"jsonrpc": "2.0", "method": "notifications/progress"}),
        ),
        ("response", json!({"jsonrpc": "2.0", "id": 3, "result": {}})),
        (
            "error response",
            json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -1, "message": "m"}}),
        ),
    ] {
        let r = rpc(rig.addr, &msg).await;
        assert_eq!(r.status, 202, "{label}: {}", r.body);
        assert_eq!(r.body, "", "{label}");
    }
    // Both string and integer numbers are ids, and the answer carries the same value (a string stays a string).
    for id in [
        json!(0),
        json!(-4),
        json!(9_007_199_254_740_991_i64),
        json!("a-b"),
        json!(""),
    ] {
        let r = rpc(rig.addr, &request(id.clone(), "ping", json!({}))).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["id"], id);
    }
    let stats = rig.shared.stats();
    assert!(stats.protocol_errors >= 13, "{}", stats.protocol_errors);
    rig.stop().await;
}

#[tokio::test]
async fn the_protocol_version_header_is_counted_and_never_a_gate() {
    let rig = Rig::start().await;
    let ping = request(json!(1), "ping", json!({}));
    for v in SUPPORTED_VERSIONS {
        let r = rpc_with(rig.addr, &[("Mcp-Protocol-Version", v)], &ping).await;
        assert_eq!(r.status, 200, "{v}");
    }
    assert_eq!(rig.shared.stats().unknown_protocol_headers, 0);
    // A value the server does not know is served as the highest version (the engine would otherwise lose its tools for the whole session).
    for v in ["1999-01-01", "2099-12-31", "", "latest"] {
        let r = rpc_with(rig.addr, &[("Mcp-Protocol-Version", v)], &ping).await;
        assert_eq!(r.status, 200, "{v:?}");
    }
    assert_eq!(rig.shared.stats().unknown_protocol_headers, 4);
    let said = rig
        .log
        .lines()
        .iter()
        .filter(|l| l.contains("protocol version"))
        .count();
    assert_eq!(said, 1, "one line a run: {}", rig.log_text());
    assert!(!rig.log_text().contains("1999-01-01"));
    rig.stop().await;
}

#[tokio::test]
async fn the_recorded_sequence_of_the_engine_is_served() {
    let rig = Rig::start().await;
    let h = [
        ("User-Agent", "Bun/1.4.2"),
        ("Mcp-Protocol-Version", "2025-11-25"),
    ];
    let init = request(
        json!(0),
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "@cline/core", "version": "0.0.0"}}),
    );
    let r = rpc_with(rig.addr, &[("User-Agent", "Bun/1.4.2")], &init).await;
    assert_eq!(r.json()["result"]["protocolVersion"], "2025-11-25");
    let r = rpc_with(
        rig.addr,
        &h,
        &notification("notifications/initialized", json!({})),
    )
    .await;
    assert_eq!((r.status, r.body.as_str()), (202, ""));
    let r = raw(
        rig.addr,
        "GET",
        "/mcp",
        &[("User-Agent", "Bun/1.4.2"), ("Accept", "text/event-stream")],
        b"",
    )
    .await;
    assert_eq!(r.status, 405);
    let r = rpc_with(rig.addr, &h, &request(json!(1), "tools/list", json!({}))).await;
    assert_eq!(r.json()["result"], tools_list_result());
    let r = rpc_with(rig.addr, &h, &search(json!(2), "echo:recorded")).await;
    assert_eq!(text_of(&r.json()), "echo: recorded");
    let s = rig.shared.stats();
    assert_eq!((s.initializes, s.lists), (1, 1));
    rig.stop().await;
}

// ---- what the server counts and what it logs -------------------------------------------------------------------------------------

#[tokio::test]
async fn the_counters_say_what_the_server_saw() {
    let rig = Rig::start().await;
    let s0 = rig.shared.stats();
    assert_eq!((s0.initializes, s0.lists), (0, 0));
    assert!(s0.calls.is_empty());
    assert_eq!(
        (s0.p50_ms, s0.p95_ms),
        (None, None),
        "no call ran: a zero would claim an instant answer"
    );
    rpc(rig.addr, &initialize(1, "2025-11-25")).await;
    rpc(rig.addr, &request(json!(2), "tools/list", json!({}))).await;
    rpc(rig.addr, &request(json!(3), "tools/list", json!({}))).await;
    rpc(rig.addr, &search(json!(4), "echo:a")).await;
    rpc(rig.addr, &search(json!(5), "echo:b")).await;
    rpc(rig.addr, &search(json!(6), "fail:timeout")).await;
    rpc(rig.addr, &search(json!(7), "fail:refused")).await;
    rpc(rig.addr, &search(json!(8), "big:50000")).await;
    rpc(
        rig.addr,
        &request(
            json!(9),
            "tools/call",
            json!({"name": "code_search", "arguments": {}}),
        ),
    )
    .await;
    rpc(
        rig.addr,
        &request(
            json!(10),
            "tools/call",
            json!({"name": "other", "arguments": {}}),
        ),
    )
    .await;
    let s = rig.shared.stats();
    assert_eq!((s.initializes, s.lists), (1, 2));
    // Calls that named a listed tool, however they ended; a call to a name the server does not list is counted nowhere.
    assert_eq!(s.calls.len(), 1);
    assert_eq!(s.calls["code_search"], 6);
    assert_eq!(s.failed, 1, "timeout is a failure of the backend");
    assert_eq!(s.refused, 2, "refused and usage did not run");
    assert_eq!(s.truncated, 1);
    assert_eq!(s.restarts, 0);
    let (p50, p95) = (s.p50_ms.unwrap(), s.p95_ms.unwrap());
    assert!(p50 <= p95);
    // The members that go to the run's summary are valid as a note.
    let counts = s.to_counts();
    assert_eq!(counts.calls["code_search"], 6);
    let note = DirectToolsNote {
        enabled: true,
        reason: None,
        port: Some(rig.addr.port()),
        connect: Some("ok".into()),
        list: rig
            .shared
            .listed()
            .iter()
            .map(ToString::to_string)
            .collect(),
        counts: Some(counts),
    };
    assert!(note.is_valid());
    rig.stop().await;
}

#[tokio::test]
async fn logs_hold_a_tool_a_word_a_time_and_a_size_and_nothing_a_user_typed() {
    let rig = Rig::start().await;
    let work = rig.work.display().to_string();
    rpc(rig.addr, &search(json!(1), "canary")).await;
    rpc(rig.addr, &search(json!(2), "SECRET-PATTERN-7")).await;
    rpc(rig.addr, &request(json!(3), "tools/call", json!({"name": "code_search", "arguments": {"SECRET-MEMBER-7": "SECRET-VALUE-7", "pattern": "SECRET-PATTERN-7"}}))).await;
    rpc(rig.addr, &search(json!(4), "big:90000")).await;
    rpc(rig.addr, &search(json!(5), "fail:unavailable")).await;
    rpc(rig.addr, &search(json!(6), "panic")).await;
    rpc(rig.addr, &search(json!(7), "cwd")).await;
    rpc(
        rig.addr,
        &request(
            json!(8),
            "tools/call",
            json!({"name": "SECRET-NAME-7", "arguments": {}}),
        ),
    )
    .await;
    rpc_with(
        rig.addr,
        &[("Mcp-Protocol-Version", "SECRET-VERSION-7")],
        &request(json!(9), "ping", json!({})),
    )
    .await;
    raw(rig.addr, "POST", "/mcp", &[], b"{SECRET-BODY-7").await;
    rpc_with(
        rig.addr,
        &[("Host", "SECRET-HOST-7.example")],
        &request(json!(10), "ping", json!({})),
    )
    .await;
    rpc_with(
        rig.addr,
        &[("Origin", "http://SECRET-ORIGIN-7.example")],
        &request(json!(11), "ping", json!({})),
    )
    .await;
    let log = rig.log_text();
    for canary in [
        "CANARY-RESULT",
        "SECRET-PATTERN-7",
        "SECRET-MEMBER-7",
        "SECRET-VALUE-7",
        "SECRET-NAME-7",
        "SECRET-VERSION-7",
        "SECRET-BODY-7",
        "SECRET-HOST-7",
        "SECRET-ORIGIN-7",
        "told to panic",
        work.as_str(),
    ] {
        assert!(!log.contains(canary), "the log holds {canary:?}:\n{log}");
    }
    // One line per call that named the tool, with the word it ended with, the milliseconds and the bytes of the text: nothing else.
    let line = regex::Regex::new(
        r"^direct tools call tool=code_search result=[a-z-]+ ms=[0-9]+ bytes=[0-9]+( truncated)?$",
    )
    .unwrap();
    let lines: Vec<String> = rig
        .log
        .lines()
        .into_iter()
        .filter(|l| l.contains("call tool="))
        .collect();
    assert_eq!(lines.len(), 7, "{log}");
    for l in &lines {
        assert!(line.is_match(l), "{l}");
    }
    let words: Vec<&str> = lines
        .iter()
        .map(|l| {
            l.split("result=")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
        })
        .collect();
    assert_eq!(
        words,
        ["ok", "ok", "usage", "ok", "unavailable", "fault", "ok"]
    );
    assert!(lines[3].ends_with(" truncated"), "{}", lines[3]);
    rig.stop().await;
}

#[tokio::test]
async fn self_check_answers_for_a_server_that_is_ours() {
    let rig = Rig::start().await;
    assert!(rig.handle().self_check().await);
    // Something else on a port is not our server.
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = other.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = other.accept().await.unwrap();
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        }
    });
    assert!(!server::self_check_addr(addr).await);
    rig.stop().await;
}
