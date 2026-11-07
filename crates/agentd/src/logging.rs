//! Log lines. Callers never pass secrets; the S3 canary check searches runtime logs.

use std::sync::{Arc, Mutex};

pub trait LogSink: Send + Sync + std::fmt::Debug {
    fn line(&self, s: &str);
}

/// Runtime logs: stderr, collected by AgentCore into the runtime log group.
#[derive(Debug, Default)]
pub struct StderrLog;

impl LogSink for StderrLog {
    fn line(&self, s: &str) {
        eprintln!("agentd: {s}");
    }
}

/// In-memory log for tests.
#[derive(Debug, Default)]
pub struct MemoryLog {
    lines: Mutex<Vec<String>>,
}

impl MemoryLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl LogSink for MemoryLog {
    fn line(&self, s: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(s.to_string());
    }
}
