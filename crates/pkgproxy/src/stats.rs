//! Counters. `requests` and `statuses` count every answer the proxy gave; `by_prefix` counts the
//! requests that reached a configured prefix; `refused` counts answers the proxy decided itself (a bad request, an unknown
//! prefix, a wrong method, an expired token, a refused redirect, a full proxy); `bytes` counts body bytes sent to clients.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub(crate) struct Stats {
    requests: AtomicU64,
    bytes: AtomicU64,
    refused: AtomicU64,
    maps: Mutex<(BTreeMap<String, u64>, BTreeMap<u16, u64>)>,
}

impl Stats {
    /// One answer, when its status is known.
    pub fn record(&self, prefix: Option<&str>, status: u16, refused: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if refused {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut m) = self.maps.lock() {
            if let Some(p) = prefix {
                *m.0.entry(p.to_string()).or_insert(0) += 1;
            }
            *m.1.entry(status).or_insert(0) += 1;
        }
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> crate::StatsSnapshot {
        let (by_prefix, statuses) = self.maps.lock().map(|m| m.clone()).unwrap_or_default();
        crate::StatsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            by_prefix,
            statuses,
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_answers_prefixes_statuses_refusals_and_bytes() {
        let s = Stats::default();
        s.record(Some("pypi"), 200, false);
        s.record(Some("pypi"), 404, false);
        s.record(None, 403, true);
        s.add_bytes(10);
        s.add_bytes(5);
        let snap = s.snapshot();
        assert_eq!((snap.requests, snap.refused, snap.bytes), (3, 1, 15));
        assert_eq!(snap.by_prefix.get("pypi"), Some(&2));
        assert_eq!(snap.statuses.get(&200), Some(&1));
        assert_eq!(snap.statuses.get(&403), Some(&1));
    }
}
