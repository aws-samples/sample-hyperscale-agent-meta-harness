//! Which event shards a watcher reads, and how long it waits (client polish B2). A run's events sit on 32 shards: shard 0 holds every
//! harness event, and agent `a` writes to `event_shard(Agent, a)`. A watcher used to read all 32 about 1.3 times a second, which is
//! 64,000 requests in 26 minutes, and for a direct run 30 of the 32 shards were always empty.
//!
//! The plan reads, each cycle, only
//! - shard 0 and the shard of every agent a `session.started` has named (hot for good),
//! - the shards that returned an event within `hot_for`,
//! - every shard that has never been read (the first cycle reads all 32, so a rehydration misses nothing), and
//! - every other shard once its last read is `cold_every` old (a safety net for an event on a shard nobody expected).
//!
//! The wait between cycles is the base while events flow and for the first three empty cycles, then doubles up to the ceiling
//! (five times the base), and an event on any shard sets it back. All three durations scale with the base, so a test runs the plan
//! in milliseconds. The plan is pure: the caller passes the time, so a test drives it with a virtual clock. It changes no wire
//! contract: a shard is read the way it always was, only less often.
//!
//! One shard is read MORE, not less (EVT1): shard 0, the harness shard, is read from its start on every cycle while the run is open, because
//! a cursor that is the largest id read cannot return an event committed later with a smaller id ([`PollPlan::cursors`]).

use crate::state::RunState;
use harness_protocol::api::v1::EventsPage;
use harness_protocol::envelope::{Authority, EventType, Payload};
use harness_protocol::store::{EVENT_SHARDS, event_shard};
use std::collections::BTreeSet;
use std::time::Duration;

/// Empty cycles that keep the base wait before the wait starts to grow.
pub const GRACE_EMPTY_CYCLES: u32 = 3;
/// The ceiling of the wait, in bases (5 s for the 1 s base).
pub const CEILING_BASES: u32 = 5;
/// A shard with no event for this many bases is read only by the cold sweep (30 s for the 1 s base).
pub const COLD_EVERY_BASES: u32 = 30;
/// A shard that returned an event stays hot for this many bases (60 s for the 1 s base).
pub const HOT_FOR_BASES: u32 = 60;
/// After the run has ended, while no session has named its model, the harness shard is read again from its start every this many cycles.
pub const SWEEP_UNKNOWN_EVERY: u32 = 3;
/// After the run has ended and a session has named its model, every this many cycles.
pub const SWEEP_EVERY: u32 = 30;
/// Cycles of reads from the start of the harness shard that follow the cycle in which the plan sees an open run end: a writer may still be
/// finishing (a delivery event of a run that was cancelled while it was delivered), and a late event is read on the next cycle.
pub const END_GRACE_CYCLES: u32 = 10;
/// The most distinct harness events the plan reads in full on every cycle. A run writes about ten; a shard with more is read after its
/// cursor, with the sweep cadence, so that one page of a long shard can never hide the rest.
pub const FULL_READ_MAX_EVENTS: usize = 100;

#[derive(Clone, Debug)]
pub struct PollPlan {
    base: Duration,
    /// Consecutive cycles that read at least one shard and found no event.
    empty: u32,
    read_at: [Option<Duration>; EVENT_SHARDS as usize],
    event_at: [Option<Duration>; EVENT_SHARDS as usize],
    /// The shards of the agents the run has started.
    agents: BTreeSet<u8>,
    /// The cycles [`PollPlan::cursors`] has planned.
    cycles: u32,
    /// [`PollPlan::cursors`] has seen the run open (no `run.completed` or `run.aborted` held). A run first seen already over has no grace.
    saw_open: bool,
    /// The cycles planned since an open run was seen to end.
    ended_cycles: u32,
    /// The ids of the harness-shard events [`PollPlan::record`] has seen. Only an id not in it counts as activity there, because the
    /// harness shard is read in full on every cycle while the run is open and returns the events already held again and again.
    seen0: BTreeSet<String>,
}

impl PollPlan {
    pub fn new(base: Duration) -> Self {
        Self {
            base,
            empty: 0,
            read_at: [None; EVENT_SHARDS as usize],
            event_at: [None; EVENT_SHARDS as usize],
            agents: BTreeSet::new(),
            cycles: 0,
            saw_open: false,
            ended_cycles: 0,
            seen0: BTreeSet::new(),
        }
    }

    /// The reads of one cycle at `now`: each chosen shard after the cursor `state` holds for it, except the harness shard. A
    /// shard's cursor is the largest event id read, and the server returns the ids above it, so an event committed later with a smaller id
    /// would never come back. The live A1 proof lost `session.started` that way (`goal.status` and `session.started` shared a millisecond and
    /// their order was the order of two hashes), and any other harness event that is committed late could be lost the same way, a
    /// completion or a link included (the Lambdas date an event by an earlier moment than the one they write it at, and control-api's cancel
    /// dates its event by the write). The harness shard holds about ten small events, so while the run is open it is read from its start on
    /// every cycle, and for [`END_GRACE_CYCLES`] cycles after the plan sees the run end. After that it is read from its start every
    /// [`SWEEP_UNKNOWN_EVERY`] cycles while no session has named its model and every [`SWEEP_EVERY`] cycles once one has; a run that is
    /// already over when first seen has no grace; a shard of more than [`FULL_READ_MAX_EVENTS`] events is read on that cadence only. The
    /// state keeps each event once (they are keyed by id) and the cursor never moves back. The agent shards keep the cursor: their ids come
    /// from one process in append order, and `worker-api` commits them in that order.
    pub fn cursors(&mut self, state: &RunState, now: Duration) -> Vec<(u8, Option<String>)> {
        self.cycles = self.cycles.saturating_add(1);
        let ended = state.ended();
        if !ended {
            self.saw_open = true;
        }
        let in_grace = ended && self.saw_open && {
            self.ended_cycles = self.ended_cycles.saturating_add(1);
            self.ended_cycles <= END_GRACE_CYCLES
        };
        let every = if state.model_seen() {
            SWEEP_EVERY
        } else {
            SWEEP_UNKNOWN_EVERY
        };
        let small = self.seen0.len() <= FULL_READ_MAX_EVENTS;
        let full = ((!ended || in_grace) && small) || self.cycles.is_multiple_of(every);
        self.shards(now)
            .into_iter()
            .map(|s| {
                let after = if s == 0 && full {
                    None
                } else {
                    state.cursor(s).map(str::to_string)
                };
                (s, after)
            })
            .collect()
    }

    /// The shards to read at `now`, in shard order.
    pub fn shards(&self, now: Duration) -> Vec<u8> {
        let hot_for = self.base * HOT_FOR_BASES;
        let cold_every = self.base * COLD_EVERY_BASES;
        (0..EVENT_SHARDS)
            .filter(|s| {
                let i = usize::from(*s);
                let hot = *s == 0
                    || self.agents.contains(s)
                    || self.event_at[i].is_some_and(|t| now.saturating_sub(t) < hot_for);
                hot || self.read_at[i].is_none_or(|t| now.saturating_sub(t) >= cold_every)
            })
            .collect()
    }

    /// Records the pages one cycle returned at `now`: the shards that answered (an empty page is an answer), the shards that gave an
    /// event, the agents a `session.started` named. A cycle in which no read answered changes nothing.
    pub fn record(&mut self, now: Duration, pages: &[(u8, EventsPage)]) {
        if pages.is_empty() {
            return;
        }
        let mut any = false;
        for (shard, page) in pages {
            let i = usize::from(*shard);
            if i >= self.read_at.len() {
                continue;
            }
            self.read_at[i] = Some(now);
            if page.events.is_empty() {
                continue;
            }
            // A read from the start returns the whole harness shard again: only an id never seen there is a sign that the run is busy
            // (a late id below the cursor counts, once).
            let fresh = if *shard == 0 {
                // Every id is noted, so not `any` (it would stop at the first new one).
                let mut fresh = false;
                for e in &page.events {
                    fresh |= self.seen0.insert(e.event_id.clone());
                }
                fresh
            } else {
                true
            };
            if fresh {
                any = true;
                self.event_at[i] = Some(now);
            }
            for e in &page.events {
                if e.event_type == EventType::SessionStarted
                    && let Some(id) = started_agent(e)
                {
                    self.agents.insert(event_shard(Authority::Agent, Some(&id)));
                }
            }
        }
        self.empty = if any { 0 } else { self.empty.saturating_add(1) };
    }

    /// The wait before the next cycle.
    pub fn interval(&self) -> Duration {
        if self.empty <= GRACE_EMPTY_CYCLES {
            return self.base;
        }
        let doublings = (self.empty - GRACE_EMPTY_CYCLES).min(16);
        (self.base * 2u32.saturating_pow(doublings)).min(self.base * CEILING_BASES)
    }
}

/// The agent a harness `session.started` names.
fn started_agent(e: &harness_protocol::envelope::Envelope) -> Option<String> {
    match &e.payload {
        Payload::Inline { value } => value
            .get("agentId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        Payload::ClaimCheck { .. } => None,
    }
    .or_else(|| e.agent_id.clone())
}
