//! The real tool server of agentd with the echo backend, for the conformance probe.
//!
//! ```text
//! tools_server [--port N]        serves 127.0.0.1:N (0, the default, takes any free port)
//! ```
//!
//! Standard output, one line each:
//!
//! | line | when |
//! |---|---|
//! | `TOOLS-SERVER READY http://127.0.0.1:<port>/mcp` | the server is up |
//! | `TOOLS-SERVER CHILD <pid>` | the backend started a child process (a `sleep` of the cancel check) |
//! | `TOOLS-SERVER RESTARTED <n>` | the serving task was started again, `n` times so far |
//! | `TOOLS-SERVER STATS <json>` | the counters, after the command `stats` |
//!
//! Commands on standard input, one per line: `crash` (abort the serving task, the way a panic would), `restart` (end it through its graceful
//! shutdown, which closes idle connections), `stats`, `stop`. End of input stops the server.

#[path = "tools_server_support/echo.rs"]
mod echo;

use agentd::direct::tools::backend::ToolContext;
use agentd::direct::tools::result::DEFAULT_RESULT_CHARS;
use agentd::direct::tools::server::{self, Shared};
use agentd::direct::tools::stats::Counters;
use agentd::logging::StderrLog;
use std::io::BufRead;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[tokio::main]
async fn main() {
    // nosemgrep: args -- the first element, the program path, is skipped and the rest is the example's own port option, read as data
    let mut args = std::env::args().skip(1);
    let mut port: u16 = 0;
    while let Some(a) = args.next() {
        if a == "--port" {
            port = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        }
    }
    let listener = match server::bind(port).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tools_server: {e}");
            std::process::exit(2);
        }
    };
    let (backend, state) = echo::Echo::new();
    // nosemgrep: temp-dir -- the conformance example only needs a scratch directory for its tool context, and no security decision reads what is written there
    let work = std::env::temp_dir();
    let ctx = ToolContext::for_directory(work, Counters::new());
    let shared = Shared::new(
        vec![backend],
        ctx,
        Arc::new(StderrLog),
        DEFAULT_RESULT_CHARS,
    );
    let handle = server::start(listener, shared.clone());
    say(&format!("TOOLS-SERVER READY http://{}/mcp", handle.addr()));

    // Report the children the backend starts, so that the probe can check that a cancel ends them.
    let watched = state.clone();
    tokio::spawn(async move {
        let mut told = 0;
        loop {
            let pids = watched.pids();
            for pid in pids.iter().skip(told) {
                say(&format!("TOOLS-SERVER CHILD {pid}"));
            }
            told = pids.len();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Standard input is read on a thread of its own (tokio's `io-std` feature is not enabled in this workspace).
    let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = commands.recv().await {
        match line.trim() {
            "crash" | "restart" => {
                let before = handle.restarts();
                if line.trim() == "crash" {
                    handle.crash_for_tests();
                } else {
                    handle.restart_gracefully_for_tests();
                }
                for _ in 0..500 {
                    if handle.restarts() > before {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                say(&format!("TOOLS-SERVER RESTARTED {}", handle.restarts()));
            }
            "stats" => {
                let s = shared.stats().to_counts();
                say(&format!(
                    "TOOLS-SERVER STATS {}",
                    serde_json::to_string(&s).unwrap_or_default()
                ));
            }
            "stop" => break,
            _ => {}
        }
    }
    handle.stop().await;
}
