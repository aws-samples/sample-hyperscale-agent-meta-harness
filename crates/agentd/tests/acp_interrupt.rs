//! The interrupt of an open prompt (package AG0, the module seam): `Interrupt` is what a module calls to cancel the prompt that is open
//! now, and `AcpClient::with_interrupt` is how the client carries it out. It writes `session/cancel` once, waits for the prompt's
//! own answer for the grace, and leaves the run-level cancel (the user's `ham stop`, which never resets) alone. In-process engine, no
//! AWS. The whole agent is in `direct_hooks.rs`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, AcpError, AcpHandler, Activity, Interrupt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

struct Quiet;

impl AcpHandler for Quiet {
    fn on_notification(&mut self, _method: &str, _params: &Value) {}
    fn on_request(&mut self, _method: &str, _params: &Value) -> Result<Value, (i64, String)> {
        Err((-32601, "not offered".into()))
    }
}

/// What the stand-in engine does with a `session/prompt`.
#[derive(Clone, Copy)]
enum OnPrompt {
    /// Answers `end_turn` at once.
    Finish,
    /// Holds the prompt until `session/cancel`, then answers `cancelled`.
    HoldUntilCancel,
    /// Never answers.
    Silent,
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;

/// A client wired to an in-process engine; the counter says how many `session/cancel` the engine read.
fn engine(on_prompt: OnPrompt) -> (Client, Arc<AtomicU32>) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let cancels = Arc::new(AtomicU32::new(0));
    let seen = cancels.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        let mut held: Option<Value> = None;
        while let Ok(Some(received)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&received).unwrap();
            let method = msg["method"].as_str().unwrap_or_default();
            if method == "session/cancel" {
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(id) = held.take() {
                    let l =
                        json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "cancelled"}});
                    out.write_all(format!("{l}\n").as_bytes()).await.unwrap();
                    out.flush().await.unwrap();
                }
                continue;
            }
            if method == "session/prompt" {
                match on_prompt {
                    OnPrompt::Finish => {
                        let l = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}});
                        out.write_all(format!("{l}\n").as_bytes()).await.unwrap();
                        out.flush().await.unwrap();
                    }
                    OnPrompt::HoldUntilCancel => held = Some(msg["id"].clone()),
                    OnPrompt::Silent => {}
                }
            }
        }
    });
    (AcpClient::new(client_r, client_w), cancels)
}

async fn prompt(client: &mut Client) -> Result<Value, AcpError> {
    client
        .request(
            "session/prompt",
            json!({"sessionId": "s-1", "prompt": [{"type": "text", "text": "go"}]}),
            &mut Quiet,
        )
        .await
}

#[tokio::test]
async fn interrupting_the_open_prompt_writes_one_cancel_and_returns_the_engines_cancelled_answer() {
    let (client, cancels) = engine(OnPrompt::HoldUntilCancel);
    let intr = Interrupt::new();
    let activity = Activity::new();
    let mut client = client
        .with_activity(activity.clone())
        .with_interrupt(intr.clone(), Duration::from_secs(10));
    assert!(!intr.prompt_open());
    let ctl = intr.clone();
    let task = tokio::spawn(async move {
        // The prompt is open as soon as the request is being written.
        while !ctl.prompt_open() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        (ctl.interrupt(), ctl.interrupt())
    });
    let answer = prompt(&mut client).await.unwrap();
    assert_eq!(answer["stopReason"], "cancelled");
    let (first, _second) = task.await.unwrap();
    assert!(first, "a prompt was open");
    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "one cancel per open prompt, however often it is asked"
    );
    assert!(activity.interrupted());
    assert!(
        activity.cancel_sent(),
        "the answer is the one agentd asked for, not the engine's own brake"
    );
    assert!(!intr.prompt_open(), "closed once the prompt is answered");
    // The next stage starts clean.
    activity.reset_for_stage();
    assert!(!activity.interrupted());
}

#[tokio::test]
async fn an_interrupt_while_no_prompt_is_open_is_refused_and_does_not_cancel_the_next_one() {
    let (client, cancels) = engine(OnPrompt::Finish);
    let intr = Interrupt::new();
    let mut client = client.with_interrupt(intr.clone(), Duration::from_secs(10));
    assert!(!intr.interrupt(), "nothing is open");
    let answer = prompt(&mut client).await.unwrap();
    assert_eq!(answer["stopReason"], "end_turn");
    assert_eq!(cancels.load(Ordering::SeqCst), 0);
    assert!(!intr.interrupt(), "closed again");
}

#[tokio::test(start_paused = true)]
async fn an_engine_that_does_not_answer_the_interrupt_ends_the_request_with_cancel_ignored_after_the_grace()
 {
    let (client, cancels) = engine(OnPrompt::Silent);
    let intr = Interrupt::new();
    let mut client = client.with_interrupt(intr.clone(), Duration::from_secs(2));
    let ctl = intr.clone();
    tokio::spawn(async move {
        while !ctl.prompt_open() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(ctl.interrupt());
    });
    let started = tokio::time::Instant::now();
    let err = prompt(&mut client).await.unwrap_err();
    assert_eq!(err, AcpError::CancelIgnored);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(3) && took < Duration::from_secs(4),
        "the grace counts from the cancel: {took:?}"
    );
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_users_cancel_and_an_interrupt_together_write_one_cancel_and_it_is_the_users() {
    let (client, cancels) = engine(OnPrompt::HoldUntilCancel);
    let intr = Interrupt::new();
    let activity = Activity::new();
    let (user_cancel, signal) = watch::channel(false);
    let mut client = client
        .with_activity(activity.clone())
        .with_cancel(signal, Duration::from_secs(10))
        .with_interrupt(intr.clone(), Duration::from_secs(10));
    let ctl = intr.clone();
    tokio::spawn(async move {
        while !ctl.prompt_open() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Both are asked in the same moment; the user's cancel is looked at first.
        user_cancel.send_replace(true);
        assert!(ctl.interrupt());
        // Keep the sender alive until the request has ended.
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(user_cancel);
    });
    let answer = prompt(&mut client).await.unwrap();
    assert_eq!(answer["stopReason"], "cancelled");
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
    assert!(activity.cancel_sent());
    assert!(
        !activity.interrupted(),
        "the cancel written was the user's, so the stage is the user's stop and not a module's"
    );
}
