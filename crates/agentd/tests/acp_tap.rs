//! The wire tap of the ACP client (package AG0, the module seam): every JSON-RPC message of the connection is shown to a `WireTap`, in
//! both directions, in the order it travelled, with a flag for a message of an abandoned session. In-process engine, no AWS. The
//! sequence numbers and the cancel event that `Hooks` adds on top are in `direct_hooks_unit.rs`; the whole agent is in `direct_hooks.rs`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, AcpHandler, Dir, WireTap};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Quiet;

impl AcpHandler for Quiet {
    fn on_notification(&mut self, _method: &str, _params: &Value) {}
    fn on_request(&mut self, _method: &str, _params: &Value) -> Result<Value, (i64, String)> {
        Err((-32601, "not offered".into()))
    }
}

#[derive(Default)]
struct Tap {
    seen: Mutex<Vec<(Dir, String, bool)>>,
}

impl WireTap for Tap {
    fn wire(&self, dir: Dir, msg: &Value, stale: bool) {
        let what = msg
            .get("method")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("response {}", msg["id"]));
        self.seen.lock().unwrap().push((dir, what, stale));
    }
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;

/// A client wired to an in-process engine that sends `before` (raw lines) when it reads the first request, then answers it.
fn engine(before: Vec<Value>) -> Client {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(received)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&received).unwrap();
            for line in &before {
                out.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            }
            if msg.get("id").is_some() {
                let l = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}});
                out.write_all(format!("{l}\n").as_bytes()).await.unwrap();
            }
            out.flush().await.unwrap();
        }
    });
    AcpClient::new(client_r, client_w)
}

fn update(session: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}}})
}

#[tokio::test]
async fn the_tap_sees_a_request_the_updates_around_it_and_the_answer_in_order_with_the_stale_flag()
{
    let tap = Arc::new(Tap::default());
    let mut client = engine(vec![update("s-old"), update("s-1")]).with_tap(tap.clone());
    client.set_session("s-1");
    let answer = client
        .request(
            "session/prompt",
            json!({"sessionId": "s-1", "prompt": [{"type": "text", "text": "go"}]}),
            &mut Quiet,
        )
        .await
        .unwrap();
    assert_eq!(answer["stopReason"], "end_turn");
    assert_eq!(
        *tap.seen.lock().unwrap(),
        [
            (Dir::ToEngine, "session/prompt".to_string(), false),
            (Dir::FromEngine, "session/update".to_string(), true),
            (Dir::FromEngine, "session/update".to_string(), false),
            (Dir::FromEngine, "response 1".to_string(), false),
        ]
    );
}

#[tokio::test]
async fn the_tap_sees_what_the_engine_sends_while_no_request_is_open() {
    let tap = Arc::new(Tap::default());
    let mut client = engine(vec![update("s-1")]).with_tap(tap.clone());
    client.set_session("s-1");
    // The engine speaks when it reads a line; a notification of ours is enough to make it send its update.
    client
        .notify("session/cancel", json!({"sessionId": "s-1"}))
        .await
        .unwrap();
    client.pump_idle(&mut Quiet).await.unwrap();
    assert_eq!(
        *tap.seen.lock().unwrap(),
        [
            (Dir::ToEngine, "session/cancel".to_string(), false),
            (Dir::FromEngine, "session/update".to_string(), false),
        ]
    );
}

#[tokio::test]
async fn a_stdout_line_that_is_not_json_rpc_is_not_a_message_and_is_not_shown() {
    let tap = Arc::new(Tap::default());
    // The engine prints a banner line (a raw JSON string is not an object with a method or an id) and then answers.
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(received)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&received).unwrap();
            out.write_all(b"engine banner: not json\n").await.unwrap();
            let l =
                json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}});
            out.write_all(format!("{l}\n").as_bytes()).await.unwrap();
            out.flush().await.unwrap();
        }
    });
    let mut client = AcpClient::new(client_r, client_w).with_tap(tap.clone());
    client
        .request("session/prompt", json!({"sessionId": "s-1"}), &mut Quiet)
        .await
        .unwrap();
    assert_eq!(
        *tap.seen.lock().unwrap(),
        [
            (Dir::ToEngine, "session/prompt".to_string(), false),
            (Dir::FromEngine, "response 1".to_string(), false),
        ]
    );
}
