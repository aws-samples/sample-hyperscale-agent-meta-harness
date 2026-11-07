//! What the tool server counts. Counts and times only: a name of a tool, a number, never a pattern, a query, a path or a result
//! (the research module's rule, `research/api.rs`). The server writes them; the hook of a later package reads a [`Stats`] snapshot for the
//! run summary (`DirectToolsNote`) and for the connect check (what the engine's client has asked for so far).

use harness_protocol::tools::{DirectToolsCounts, ToolName};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The latest this many latencies are kept for the percentiles, so a long run does not grow without bound.
pub const MAX_LATENCY_SAMPLES: usize = 4096;

/// The live counters. Shared between the server, the tools (through `ToolContext`) and the hook.
#[derive(Debug, Default)]
pub struct Counters {
    initializes: AtomicU64,
    lists: AtomicU64,
    failed: AtomicU64,
    refused: AtomicU64,
    truncated: AtomicU64,
    cancelled: AtomicU64,
    protocol_errors: AtomicU64,
    unknown_protocol_headers: AtomicU64,
    restarts: AtomicU32,
    calls: Mutex<BTreeMap<&'static str, u64>>,
    latencies: Mutex<VecDeque<u64>>,
}

/// A snapshot of the counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `initialize` requests the server answered.
    pub initializes: u64,
    /// `tools/list` requests the server answered.
    pub lists: u64,
    /// `tools/call` requests that named a listed tool, per bare name, however they ended. A name the server does not list is counted nowhere.
    pub calls: BTreeMap<String, u64>,
    /// Calls whose backend failed (`timeout`, `unavailable`, an internal fault, the server's backstop).
    pub failed: u64,
    /// Calls the server did not run (`usage`, `busy`, `refused`, `disabled`).
    pub refused: u64,
    /// Calls whose answer was shortened, by the backend or by the server's cap.
    pub truncated: u64,
    /// Calls the client or the server cancelled. They are in `calls`; they are neither failed nor refused, and they have no latency.
    pub cancelled: u64,
    /// Requests the server could not read (HTTP 400 and 413 with a JSON-RPC error). The engine sends none, so one means drift.
    pub protocol_errors: u64,
    /// Requests that carried an `Mcp-Protocol-Version` the server does not know (it served them as the highest version).
    pub unknown_protocol_headers: u64,
    /// Times the serving task ended and was started again.
    pub restarts: u32,
    /// The latency of the calls that ran, in milliseconds; absent when no call ran (a zero would claim an instant answer).
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

impl Stats {
    /// The members of the run summary's `counts` block.
    pub fn to_counts(&self) -> DirectToolsCounts {
        DirectToolsCounts {
            calls: self.calls.clone(),
            failed: self.failed,
            refused: self.refused,
            truncated: self.truncated,
            restarts: self.restarts,
            p50_ms: self.p50_ms,
            p95_ms: self.p95_ms,
        }
    }
}

impl Counters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record_initialize(&self) {
        self.initializes.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_list(&self) {
        self.lists.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_call(&self, tool: ToolName) {
        *self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(tool.as_str())
            .or_insert(0) += 1;
    }

    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_refused(&self) {
        self.refused.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_truncated(&self) {
        self.truncated.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_cancelled(&self) {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_protocol_error(&self) {
        self.protocol_errors.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_unknown_protocol_header(&self) {
        self.unknown_protocol_headers.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_restart(&self) {
        self.restarts.fetch_add(1, Ordering::SeqCst);
    }

    /// The time one call took. Rounded up to the millisecond, so a call that ran is never recorded as 0.
    pub fn record_latency(&self, d: Duration) {
        let ms = u64::try_from(d.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
        let mut l = self.latencies.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() >= MAX_LATENCY_SAMPLES {
            l.pop_front();
        }
        l.push_back(ms);
    }

    pub fn snapshot(&self) -> Stats {
        let mut sorted: Vec<u64> = self
            .latencies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        sorted.sort_unstable();
        Stats {
            initializes: self.initializes.load(Ordering::SeqCst),
            lists: self.lists.load(Ordering::SeqCst),
            calls: self
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect(),
            failed: self.failed.load(Ordering::SeqCst),
            refused: self.refused.load(Ordering::SeqCst),
            truncated: self.truncated.load(Ordering::SeqCst),
            cancelled: self.cancelled.load(Ordering::SeqCst),
            protocol_errors: self.protocol_errors.load(Ordering::SeqCst),
            unknown_protocol_headers: self.unknown_protocol_headers.load(Ordering::SeqCst),
            restarts: self.restarts.load(Ordering::SeqCst),
            p50_ms: nearest_rank(&sorted, 50),
            p95_ms: nearest_rank(&sorted, 95),
        }
    }
}

/// The nearest-rank percentile of an ascending list: the `ceil(p * n / 100)`-th value. `None` for an empty list.
fn nearest_rank(sorted: &[u64], percent: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}
