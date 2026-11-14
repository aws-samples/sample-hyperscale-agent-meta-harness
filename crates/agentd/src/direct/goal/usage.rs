//! What the engine spent outside every answered prompt.
//!
//! The pinned engine reports usage only in the answer of a `session/prompt`: the difference of its session total across that prompt. A prompt
//! that ends in an error has no answer to carry it, and the follow-up rounds the engine's goal plugin queues have no prompt at all, so their
//! tokens were not in the run's usage (the run of 2026-11-11 showed 67.6% of the metered cost). The observer writes each engine run's own
//! counts on its end line ([`super::rounds::parse_usage`]); this part adds up the runs that ended outside every prompt whose answer reported
//! usage, and the driver adds that to what the prompts reported.
//!
//! A run is matched to a prompt by time. The engine writes a run's end line before it answers the prompt that started the run, so the end of
//! that run lies between the moment the prompt was sent and the moment its answer arrived; the two times come from the clock of the machine
//! the engine runs on. A run that ended inside such a window is already in that prompt's figure. Every other run with counts is added: the run
//! of a prompt that ended in an error or whose answer had no usage, and every round the engine started by itself. The sum is taken when the
//! engine is gone, so the order in which end lines and answers were read does not matter, and a prompt still open then counts as unanswered.
//!
//! The rounds log is the engine user's to write: each line is bounded ([`super::rounds::MAX_RUN_TOKENS`]) and what it adds is only ever added
//! on top of what the prompts reported. No outcome, label, limit or delivery depends on it.

use crate::direct::summary::add_usage;
use harness_protocol::common::Usage;
use std::collections::BTreeMap;

use super::rounds::RunUsage;

/// The most runs kept; a run with more adds the first ones only (each entry is a few dozen bytes).
pub const MAX_RUNS: usize = 100_000;
/// The reads of the rounds log made when the engine is gone (each at most `rounds::POLL_BYTES`).
pub const FINAL_READS: usize = 64;

/// Milliseconds since the epoch on this machine's clock, the clock the observer writes `t` with.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The prompts of a run and the runs of its engine, for [`OutsideUsage::total`].
#[derive(Debug, Default)]
pub struct OutsideUsage {
    /// Prompts sent and not answered yet: their number and when they went out.
    open: BTreeMap<u32, u64>,
    /// From sending to answer, for every prompt whose answer reported usage.
    reported: Vec<(u64, u64)>,
    /// Every end line that carried counts, in the order read.
    runs: Vec<RunUsage>,
    /// The last answer added the last window of `reported`.
    last_covers: bool,
}

impl OutsideUsage {
    /// Prompt `no` went out at `at_ms`.
    pub fn prompt_sent(&mut self, no: u32, at_ms: u64) {
        self.last_covers = false;
        self.open.insert(no, at_ms);
    }

    /// Prompt `no` was answered at `at_ms` with `usage`, the usage of the answer (`None` for an error or an answer without usage). Only an
    /// answer with usage that is not all zero covers its window: an answer of zeros added nothing to what the prompts reported. An answer to
    /// a prompt that was never seen as sent changes nothing.
    pub fn prompt_replied(&mut self, no: u32, at_ms: u64, usage: Option<Usage>) {
        self.last_covers = false;
        if let Some(sent) = self.open.remove(&no)
            && usage.is_some_and(|u| u != Usage::default())
        {
            self.reported.push((sent, at_ms));
            self.last_covers = true;
        }
    }

    /// The retry loop dropped the try just answered and goes on (the engine ended it `cancelled` by itself, and the harness resumes, or the
    /// route probe found the route down, or the user cancelled while it waited): its usage is not in what the prompts reported, so its window
    /// no longer covers its run.
    pub fn try_dropped(&mut self) {
        if std::mem::take(&mut self.last_covers) {
            self.reported.pop();
        }
    }

    /// An engine run ended with these counts.
    pub fn run_ended(&mut self, run: RunUsage) {
        if self.runs.len() < MAX_RUNS {
            self.runs.push(run);
        }
    }

    /// The sum of the runs that ended outside every window of a prompt whose answer reported usage; `None` when there is none, so a run with
    /// nothing outside its prompts keeps the usage it had. A run that spent nothing (it failed before its first model response) adds
    /// nothing and is not "none": a run whose only outside runs are empty keeps the usage it had too, not measured zeros.
    pub fn total(&self) -> Option<Usage> {
        self.runs
            .iter()
            .filter(|r| {
                !self
                    .reported
                    .iter()
                    .any(|(from, to)| *from <= r.t_ms && r.t_ms <= *to)
            })
            .map(|r| r.usage)
            .filter(|u| *u != Usage::default())
            .reduce(add_usage)
    }
}
