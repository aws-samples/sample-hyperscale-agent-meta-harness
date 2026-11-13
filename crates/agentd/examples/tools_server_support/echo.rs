//! The echo backend of the tool server's tests and of the conformance example.
//!
//! It is the one tool of the protocol table (`code_search`) with a body that does what a test asks, so that the server can be driven without
//! `rg`: the `pattern` member says what to do. The tests include this file with `#[path]` and the example `tools_server` does the same, so the
//! two exercise one backend.
//!
//! | `pattern` | answer |
//! |---|---|
//! | `echo:<text>` or any other text | `echo: <text>` |
//! | `cwd` | the work tree of the context |
//! | `big:<n>` | `n` letters |
//! | `rep:<n>:<unit>` | `unit` repeated `n` times |
//! | `lines:<n>` | `n` lines like `src/file.rs:12: let x = 1;` |
//! | `trunc` | a short text the backend itself marks as shortened |
//! | `canary` | text that holds the word `CANARY-RESULT` (the log tests look for it) |
//! | `fail:<word>` | the failure `<word>` (one of the six) with the detail `test` |
//! | `sleep:<secs>` | a `sleep` child in a group of its own; ends the child and answers `error: unavailable` when cancelled |
//! | `stubborn:<secs>` | the same child but ignores the cancel signal (only the abort or a drop stops it) |
//! | `gate` | waits until the test calls `EchoState::release` |
//! | `hang` | never answers |
//! | `panic` | panics |

#![allow(dead_code, clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::direct::tools::backend::{
    CancelToken, FailureWord, GroupGuard, ToolBackend, ToolContext, ToolOutput, kill_process_group,
};
use harness_protocol::tools::ToolName;
use serde_json::Value;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::Notify;

/// What the tests read back from the backend.
#[derive(Default)]
pub struct EchoState {
    /// The pid of every child the backend started, in order.
    pub pids: Mutex<Vec<u32>>,
    /// How many calls have entered the backend.
    pub entered: AtomicUsize,
    gate: Notify,
}

impl EchoState {
    /// Lets every call that waits at `gate` go on.
    pub fn release(&self) {
        self.gate.notify_waiters();
    }

    pub fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }
}

pub struct Echo {
    pub state: Arc<EchoState>,
}

impl Echo {
    /// The backend, boxed as the server takes it, and the state the caller reads back.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (Box<dyn ToolBackend>, Arc<EchoState>) {
        let state = Arc::new(EchoState::default());
        (
            Box::new(Echo {
                state: state.clone(),
            }),
            state,
        )
    }
}

fn word(s: &str) -> FailureWord {
    FailureWord::ALL
        .iter()
        .copied()
        .find(|w| w.as_str() == s)
        .unwrap_or(FailureWord::Unavailable)
}

impl ToolBackend for Echo {
    fn name(&self) -> ToolName {
        ToolName::CodeSearch
    }

    fn call<'a>(
        &'a self,
        ctx: &'a ToolContext,
        input: Value,
        cancel: CancelToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            self.state.entered.fetch_add(1, Ordering::SeqCst);
            let pattern = input
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let (verb, rest) = pattern.split_once(':').unwrap_or((pattern.as_str(), ""));
            match verb {
                "cwd" => ToolOutput::text(ctx.work_tree.display().to_string()),
                "big" => ToolOutput::text("a".repeat(rest.parse().unwrap_or(0))),
                "rep" => {
                    let (n, unit) = rest.split_once(':').unwrap_or(("0", ""));
                    ToolOutput::text(unit.repeat(n.parse().unwrap_or(0)))
                }
                "lines" => {
                    let n: usize = rest.parse().unwrap_or(0);
                    let mut s = String::new();
                    for i in 0..n {
                        s.push_str(&format!("src/file.rs:{i}: let x = {i};\n"));
                    }
                    ToolOutput::text(s)
                }
                "trunc" => ToolOutput::shortened("partial"),
                "canary" => ToolOutput::text("found CANARY-RESULT here"),
                "fail" => ToolOutput::failed(word(rest), "test"),
                "sleep" => self.sleep(rest, &cancel, true).await,
                "stubborn" => self.sleep(rest, &cancel, false).await,
                "gate" => {
                    let waiting = self.state.gate.notified();
                    tokio::pin!(waiting);
                    waiting.as_mut().enable();
                    waiting.await;
                    ToolOutput::text("released")
                }
                "hang" => std::future::pending().await,
                "panic" => panic!("the echo backend was told to panic"),
                "echo" => ToolOutput::text(format!("echo: {rest}")),
                _ => ToolOutput::text(format!("echo: {pattern}")),
            }
        })
    }
}

impl Echo {
    /// A `sleep` child in a group of its own. `obeys`: the backend reacts to the cancel signal (it kills the group and reaps the child); when
    /// not, it only waits, and the abort of the task (a dropped future) is what stops the child.
    async fn sleep(&self, secs: &str, cancel: &CancelToken, obeys: bool) -> ToolOutput {
        let mut child = match Command::new("sleep")
            .arg(secs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return ToolOutput::failed(FailureWord::Unavailable, "no sleep"),
        };
        let Some(pid) = child.id() else {
            return ToolOutput::failed(FailureWord::Unavailable, "no pid");
        };
        self.state.pids.lock().unwrap().push(pid);
        // A dropped future (an abort, a client that hung up) kills the group; the guard is disarmed once the child is reaped, because the number
        // could be handed to another process after that.
        let mut guard = GroupGuard::new(pid);
        let out = if obeys {
            tokio::select! {
                _ = child.wait() => ToolOutput::text("slept"),
                () = cancel.cancelled() => {
                    kill_process_group(pid).await;
                    let _ = child.wait().await;
                    ToolOutput::failed(FailureWord::Unavailable, "cancelled")
                }
            }
        } else {
            let _ = child.wait().await;
            ToolOutput::text("slept")
        };
        guard.disarm();
        out
    }
}
