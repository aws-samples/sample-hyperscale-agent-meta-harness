//! The note the tool hook leaves in `summary.json`: `DirectSummary.tools`.
//!
//! Words and counts only: the reason the module is off, the loopback port, the connect word, the bare names of the tools listed, and the counters of
//! the server. Nothing a user or a model typed ever reaches it, because nothing in [`Stats`] holds such text.

use crate::direct::tools::connect::Connect;
use crate::direct::tools::stats::Stats;
use harness_protocol::tools::DirectToolsNote;

/// The module decided to be off, or its server was stopped for good: `enabled` is false and `reason` says why in one kebab word.
pub fn off(reason: &str) -> DirectToolsNote {
    DirectToolsNote {
        enabled: false,
        reason: Some(reason.to_string()),
        port: None,
        connect: None,
        list: Vec::new(),
        counts: None,
    }
}

/// The module started its server. `stopped` is the word of a server that is down for good (`restarts-used`), left out while it serves.
pub fn running(
    port: u16,
    connect: Option<Connect>,
    list: Vec<String>,
    stats: &Stats,
    stopped: Option<&str>,
) -> DirectToolsNote {
    DirectToolsNote {
        enabled: true,
        reason: stopped.map(str::to_string),
        port: Some(port),
        connect: connect.map(|c| c.as_str().to_string()),
        list,
        counts: Some(stats.to_counts()),
    }
}
