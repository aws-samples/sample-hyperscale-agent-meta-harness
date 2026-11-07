//! The fake engine's MCP client against a small HTTP server of this test.
//!
//! The client stands in for the part of the pinned engine that reads its tool servers from a settings file at the first prompt of a session and
//! calls them. It is only worth having if it does what the engine does and nothing the engine would not: these tests pin the requests it sends
//! (the order, the headers a server such as agentd's checks), how far each `mcp-connect` level goes, that a session connects once, what a call
//! does to the update it belongs to, and that a prompt without an `mcp` line touches neither the file nor the network.
#![allow(clippy::unwrap_used)]

use ham_e2e::fake_mcp::McpSim;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What the server saw: the JSON body and the header block of each request.
type Seen = Arc<Mutex<Vec<(Value, String)>>>;

/// Serves canned JSON-RPC answers on a loopback port until the test ends; `answer` gets the request and returns the HTTP status and the body.
fn serve(answer: impl Fn(&Value) -> (u16, String) + Send + 'static) -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body) = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (String::new(), Vec::new());
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..at]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    let mut body = buf[at + 4..].to_vec();
                    while body.len() < len {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                    break (head, body);
                }
            };
            let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            log.lock().unwrap().push((req.clone(), head));
            let (status, text) = answer(&req);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    (addr, seen)
}

fn good_server() -> (SocketAddr, Seen) {
    serve(|req| {
        match req["method"].as_str() {
        Some("initialize") => (200, json!({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2025-11-25"}}).to_string()),
        Some("notifications/initialized") => (202, String::new()),
        Some("tools/list") => (
            200,
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "code_search"}]}}).to_string(),
        ),
        Some("tools/call") => (
            200,
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": format!("answer to {}", req["params"]["arguments"]["pattern"].as_str().unwrap_or(""))}], "isError": false}}).to_string(),
        ),
        _ => (200, json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "no such method"}}).to_string()),
    }
    })
}

fn settings_file(addr: SocketAddr) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fakemcp-{}-{}", std::process::id(), addr.port()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cline_mcp_settings.json");
    std::fs::write(
        &path,
        json!({"mcpServers": {"ham": {"transport": {"type": "streamableHttp", "url": format!("http://{addr}/mcp")}, "timeout": 120}}}).to_string(),
    )
    .unwrap();
    path
}

fn message(update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": update}})
}

/// A transcript tool call and its update, as the replay holds them.
fn pair(id: &str, title: &str) -> Vec<Value> {
    vec![
        message(
            json!({"sessionUpdate": "tool_call", "kind": "other", "status": "pending", "title": title, "toolCallId": id}),
        ),
        message(
            json!({"sessionUpdate": "tool_call_update", "status": "completed", "toolCallId": id}),
        ),
    ]
}

fn replay(parts: &[(&str, &str)]) -> Vec<Value> {
    let mut out: Vec<Value> = parts
        .iter()
        .flat_map(|(id, title)| pair(id, title))
        .collect();
    out.push(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}));
    out
}

fn methods(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|(r, _)| r["method"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn run(sim: &mut McpSim, out: &mut Vec<Value>, sid: &str) -> Vec<String> {
    let mut lines = Vec::new();
    sim.resolve(out, sid, &mut |l| lines.push(l.to_string()));
    lines
}

fn update_of<'a>(out: &'a [Value], id: &str) -> &'a Value {
    out.iter()
        .find(|m| {
            m.pointer("/params/update/toolCallId")
                .and_then(Value::as_str)
                == Some(id)
                && m.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("tool_call_update")
        })
        .unwrap()
}

#[test]
fn a_prompt_without_an_mcp_line_touches_neither_the_file_nor_the_network() {
    let (addr, seen) = good_server();
    let mut sim = McpSim::new(Some(settings_file(addr)));
    let mut out = replay(&[
        ("c1", "Read src/calc.py"),
        ("c2", "skills: lifecycle-canary"),
    ]);
    let before = out.clone();
    let lines = run(&mut sim, &mut out, "s1");
    assert_eq!(out, before);
    assert!(lines.is_empty(), "{lines:?}");
    assert!(methods(&seen).is_empty());
}

#[test]
fn full_connects_lists_and_calls_in_the_order_an_engine_does() {
    let (addr, seen) = good_server();
    let mut sim = McpSim::new(Some(settings_file(addr)));
    let mut out = replay(&[("c1", "mcp: ham/code_search {\"pattern\": \"echo:hi\"}")]);
    let lines = run(&mut sim, &mut out, "s1");
    assert_eq!(
        methods(&seen),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    let u = update_of(&out, "c1");
    assert_eq!(
        u.pointer("/params/update/status"),
        Some(&json!("completed"))
    );
    assert_eq!(
        u.pointer("/params/update/content/0/content/text"),
        Some(&json!("answer to echo:hi"))
    );
    // What the log says: counts and words.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("mcp connect full url=http://127.0.0.1:")),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"mcp initialize 200".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"mcp tools/list 200 tools=1".to_string()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("mcp call code_search completed chars=")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("answer to")),
        "a log line holds a result: {lines:?}"
    );
    // The requests are what a server such as agentd's accepts: a loopback Host, JSON in, both answers acceptable, the length.
    let all: Vec<(Value, String)> = seen.lock().unwrap().clone();
    for (req, head) in &all {
        let head = head.to_ascii_lowercase();
        assert!(head.starts_with("post /mcp http/1.1"), "{head}");
        assert!(head.contains(&format!("host: {addr}")), "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");
        assert!(
            head.contains("accept: application/json, text/event-stream"),
            "{head}"
        );
        assert_eq!(req["jsonrpc"], json!("2.0"));
    }
    assert_eq!(all[0].0["params"]["protocolVersion"], json!("2025-11-25"));
    assert_eq!(
        all[3].0["params"],
        json!({"name": "code_search", "arguments": {"pattern": "echo:hi"}})
    );
}

#[test]
fn the_connect_levels_go_exactly_as_far_as_they_say() {
    for (level, want, call_ok) in [
        ("none", &[][..], false),
        (
            "handshake-only",
            &["initialize", "notifications/initialized"][..],
            false,
        ),
        (
            "full",
            &[
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call",
            ][..],
            true,
        ),
    ] {
        let (addr, seen) = good_server();
        let mut sim = McpSim::new(Some(settings_file(addr)));
        let mut out = replay(&[
            ("d1", &format!("mcp-connect: {level}")),
            ("c1", "mcp: ham/code_search {\"pattern\": \"x\"}"),
        ]);
        let lines = run(&mut sim, &mut out, "s1");
        assert_eq!(methods(&seen), want, "{level}");
        let u = update_of(&out, "c1");
        let status = u
            .pointer("/params/update/status")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(status == "completed", call_ok, "{level}: {u}");
        if !call_ok {
            assert_eq!(
                u.pointer("/params/update/content/0/content/text"),
                Some(&json!("tool not available: ham__code_search")),
                "{level}"
            );
            assert!(
                lines.contains(&"mcp call code_search not-connected chars=0".to_string()),
                "{lines:?}"
            );
        }
        // The directive is not a tool call: neither message of its pair is left.
        assert!(
            out.iter().all(|m| m
                .pointer("/params/update/toolCallId")
                .and_then(Value::as_str)
                != Some("d1")),
            "{level}: the directive reached the client"
        );
    }
}

#[test]
fn a_session_connects_once_and_a_new_session_connects_again() {
    let (addr, seen) = good_server();
    let mut sim = McpSim::new(Some(settings_file(addr)));
    for sid in ["s1", "s1", "s2"] {
        let mut out = replay(&[("c1", "mcp: ham/code_search {\"pattern\": \"x\"}")]);
        run(&mut sim, &mut out, sid);
    }
    let m = methods(&seen);
    assert_eq!(m.iter().filter(|x| *x == "initialize").count(), 2, "{m:?}");
    assert_eq!(m.iter().filter(|x| *x == "tools/list").count(), 2, "{m:?}");
    assert_eq!(m.iter().filter(|x| *x == "tools/call").count(), 3, "{m:?}");
}

#[test]
fn the_engine_may_damage_its_file_after_it_connected() {
    for (act, word) in [("edit", "edited"), ("delete", "deleted")] {
        let (addr, _seen) = good_server();
        let file = settings_file(addr);
        let mut sim = McpSim::new(Some(file.clone()));
        let mut out = replay(&[
            ("d1", &format!("mcp-settings: {act}")),
            ("c1", "mcp: ham/code_search {\"pattern\": \"x\"}"),
        ]);
        let lines = run(&mut sim, &mut out, "s1");
        // It connected first (the call completed) and damaged the file afterwards.
        assert_eq!(
            update_of(&out, "c1").pointer("/params/update/status"),
            Some(&json!("completed")),
            "{act}"
        );
        assert!(lines.contains(&format!("mcp settings {word}")), "{lines:?}");
        match act {
            "edit" => assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "{\"mcpServers\":{}}\n"
            ),
            _ => assert!(!file.exists()),
        }
        // A new session now finds nothing to connect to.
        let mut next = replay(&[("c2", "mcp: ham/code_search {\"pattern\": \"x\"}")]);
        let lines = run(&mut sim, &mut next, "s2");
        assert!(
            lines.iter().any(|l| l.contains("unreadable-settings")),
            "{act}: {lines:?}"
        );
        assert_eq!(
            update_of(&next, "c2").pointer("/params/update/status"),
            Some(&json!("failed"))
        );
    }
}

#[test]
fn a_server_that_answers_an_error_or_nothing_fails_the_update_with_its_words() {
    // A JSON-RPC error: the message is the update's text.
    let (addr, _) = serve(|req| {
        match req["method"].as_str() {
        Some("tools/call") => (200, json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32602, "message": "unknown tool"}}).to_string()),
        Some("tools/list") => (200, json!({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": []}}).to_string()),
        _ => (200, json!({"jsonrpc": "2.0", "id": req["id"], "result": {}}).to_string()),
    }
    });
    let mut sim = McpSim::new(Some(settings_file(addr)));
    let mut out = replay(&[("c1", "mcp: ham/nope {}")]);
    let lines = run(&mut sim, &mut out, "s1");
    let u = update_of(&out, "c1");
    assert_eq!(u.pointer("/params/update/status"), Some(&json!("failed")));
    assert_eq!(
        u.pointer("/params/update/content/0/content/text"),
        Some(&json!("unknown tool"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("mcp call nope failed chars=12")),
        "{lines:?}"
    );

    // A server that answers `initialize` with an error status: the connect did not happen, so the call is not sent.
    let (addr, seen) = serve(|_| (500, "boom".to_string()));
    let mut sim = McpSim::new(Some(settings_file(addr)));
    let mut out = replay(&[("c1", "mcp: ham/code_search {\"pattern\": \"x\"}")]);
    let lines = run(&mut sim, &mut out, "s1");
    assert_eq!(
        methods(&seen),
        ["initialize"],
        "a failed initialize ends the connect: {lines:?}"
    );
    assert_eq!(
        update_of(&out, "c1").pointer("/params/update/status"),
        Some(&json!("failed"))
    );
    assert!(
        lines.contains(&"mcp initialize 500".to_string()),
        "{lines:?}"
    );
}

#[test]
fn no_settings_file_is_a_connect_of_none() {
    let mut sim = McpSim::new(None);
    let mut out = replay(&[("c1", "mcp: ham/code_search {\"pattern\": \"x\"}")]);
    let lines = run(&mut sim, &mut out, "s1");
    assert!(
        lines.iter().any(|l| l.contains("unreadable-settings")),
        "{lines:?}"
    );
    assert_eq!(
        update_of(&out, "c1").pointer("/params/update/status"),
        Some(&json!("failed"))
    );
}
