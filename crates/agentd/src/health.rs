//! `/ping` state: `HealthyBusy` while any turn runs, so AgentCore keeps the session.

use crate::time::now_unix;
use harness_protocol::runtime::{PingResponse, PingStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug)]
pub struct Health {
    busy: AtomicUsize,
    last_update: AtomicU64,
}

impl Health {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            busy: AtomicUsize::new(0),
            last_update: AtomicU64::new(now_unix() as u64),
        })
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    pub fn ping(&self) -> PingResponse {
        PingResponse {
            status: if self.is_busy() {
                PingStatus::HealthyBusy
            } else {
                PingStatus::Healthy
            },
            time_of_last_update: self.last_update.load(Ordering::SeqCst),
        }
    }

    /// Marks one turn running until the guard drops.
    pub fn busy(this: &Arc<Self>) -> BusyGuard {
        this.busy.fetch_add(1, Ordering::SeqCst);
        this.touch();
        BusyGuard(this.clone())
    }

    fn touch(&self) {
        self.last_update.store(now_unix() as u64, Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub struct BusyGuard(Arc<Health>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}
