//! The two pure pieces of the module's state: the queue of hand-outs agentd has seen but not yet claimed, and the continuation
//! allowance. Neither touches the engine, the clock of the process or the network; the hook (`mod.rs`) feeds them facts.

use crate::direct::messages::types::{Mode, Notice};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

/// How many ids the queue remembers as already processed. A run accepts at most 100 messages; the margin is for
/// hand-outs of a message that was withdrawn, held and confirmed again.
const SEEN_LIMIT: usize = 4096;

/// What one `offer` did with a hand-out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offered {
    pub added: usize,
    /// Already known (queued, claimed or finished): the repeated hand-out, processed once.
    pub duplicate: usize,
    /// The queue was full. The server keeps the message eligible and hands it out again.
    pub overflow: usize,
}

/// The messages the control plane handed out and agentd has not claimed. Hand-out only transports: a message can be handed out
/// many times, so the queue remembers every id it has taken and takes each once. The claim is the authority, not this queue.
#[derive(Debug)]
pub struct Queue {
    cap: usize,
    items: VecDeque<Notice>,
    seen: HashSet<String>,
    /// Arrival counter, the order of messages that carry no server sequence.
    arrivals: u64,
    order: std::collections::HashMap<String, u64>,
}

impl Queue {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            items: VecDeque::new(),
            seen: HashSet::new(),
            arrivals: 0,
            order: std::collections::HashMap::new(),
        }
    }

    /// Takes the hand-out. `now` messages go before `next` ones, and within a mode the server's order decides (arrival order when the
    /// server gave none).
    pub fn offer(&mut self, notices: Vec<Notice>) -> Offered {
        let mut r = Offered::default();
        for n in notices {
            if self.seen.contains(&n.id) {
                r.duplicate += 1;
                continue;
            }
            if self.items.len() >= self.cap {
                r.overflow += 1;
                continue;
            }
            if self.seen.len() < SEEN_LIMIT {
                self.seen.insert(n.id.clone());
            }
            self.arrivals += 1;
            self.order.insert(n.id.clone(), self.arrivals);
            self.items.push_back(n);
            r.added += 1;
        }
        r
    }

    fn rank(&self, n: &Notice) -> (u8, u64, u64) {
        let mode = match n.mode {
            Mode::Now => 0,
            Mode::Next => 1,
        };
        (mode, n.seq, self.order.get(&n.id).copied().unwrap_or(0))
    }

    fn best(&self) -> Option<usize> {
        self.items
            .iter()
            .enumerate()
            .min_by_key(|(_, n)| self.rank(n))
            .map(|(i, _)| i)
    }

    pub fn peek(&self) -> Option<&Notice> {
        self.best().and_then(|i| self.items.get(i))
    }

    /// The next message to claim, removed from the queue.
    pub fn take_next(&mut self) -> Option<Notice> {
        let i = self.best()?;
        let n = self.items.remove(i)?;
        self.order.remove(&n.id);
        Some(n)
    }

    /// Whether the id is queued now.
    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|n| n.id == id)
    }

    /// Takes a queued message out by id and returns it (the claim of an interrupt, which does not wait for the turn end).
    pub fn take(&mut self, id: &str) -> Option<Notice> {
        let i = self.items.iter().position(|n| n.id == id)?;
        let n = self.items.remove(i)?;
        self.order.remove(id);
        Some(n)
    }

    /// Puts a message back that was taken and could not be claimed (the store could not answer): it keeps its place by server order,
    /// ahead of anything that arrived after it, and is not counted as new.
    pub fn requeue(&mut self, n: Notice) {
        self.arrivals += 1;
        // earlier than every arrival, so it goes first among messages that carry no server order
        self.order.insert(n.id.clone(), 0);
        self.items.push_front(n);
    }

    /// A `now` message that cannot interrupt becomes a `next` message: it goes out at the next turn end, in server order among the
    /// `next` messages (first among those that carry no server order).
    pub fn demote(&mut self, id: &str) {
        if let Some(n) = self.items.iter_mut().find(|n| n.id == id) {
            n.mode = Mode::Next;
            self.order.insert(id.to_string(), 0);
        }
    }

    /// Takes a message out by id (it was withdrawn, or another call settled it). `true` when it was queued.
    pub fn remove(&mut self, id: &str) -> bool {
        match self.items.iter().position(|n| n.id == id) {
            Some(i) => {
                self.items.remove(i);
                self.order.remove(id);
                true
            }
            None => false,
        }
    }

    /// The session changed: what was written against another session leaves the queue (the server holds it for the user to confirm).
    pub fn retain_epoch(&mut self, epoch: &str) -> Vec<Notice> {
        let (keep, drop): (Vec<Notice>, Vec<Notice>) =
            self.items.drain(..).partition(|n| n.epoch == epoch);
        for n in &drop {
            self.order.remove(&n.id);
        }
        self.items = keep.into();
        drop
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn has_now(&self) -> bool {
        self.items.iter().any(|n| n.mode == Mode::Now)
    }

    pub fn has_next_mode(&self) -> bool {
        self.items.iter().any(|n| n.mode == Mode::Next)
    }

    /// Whether this id was ever taken.
    pub fn knows(&self, id: &str) -> bool {
        self.seen.contains(id)
    }
}

/// What the allowance says about a message at a turn end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// The turns or the time a stage set for the allowance are spent: the message ends rejected for the limit.
    ContinuationLimit,
    /// Too little of the run's time is left to start another turn: the message ends rejected, the run is sealed.
    TooLate,
}

/// The continuation after done. It starts at the first done-looking stop, and a turn starts only with `min_left` of the run's
/// time left. No count of turns or of execution time binds it unless a stage sets one:
/// the run deadline and `min_left` end it. A model change, an interrupt, an answer, a session replacement and another done-looking stop do not
/// reset the counts that are kept. Messages delivered before the first done-looking stop (the engine was still working) are not counted.
#[derive(Clone, Debug)]
pub struct Budget {
    max_turns: Option<u32>,
    max_exec: Option<Duration>,
    min_left: Duration,
    started: bool,
    turns: u32,
    exec: Duration,
    /// The turn in flight started inside the allowance, so its time counts.
    in_flight: bool,
}

impl Budget {
    /// No count of turns, no limit on execution time, 300 s left. The driver's deadline already holds the 900 s reserve for delivery
    /// (`MAX_DIRECT_TIMEOUT_S`), so the 300 s is what must remain on top of it.
    pub fn standard() -> Self {
        Self::new(None, None, Duration::from_secs(300))
    }

    /// A budget with the counts a stage may set; `None` leaves that count off.
    pub fn new(max_turns: Option<u32>, max_exec: Option<Duration>, min_left: Duration) -> Self {
        Self {
            max_turns,
            max_exec,
            min_left,
            started: false,
            turns: 0,
            exec: Duration::ZERO,
            in_flight: false,
        }
    }

    /// A stop of the engine's turn. The first one that looks like done starts the allowance; later ones change nothing.
    pub fn note_stop(&mut self, done_looking: bool) {
        if done_looking {
            self.started = true;
        }
    }

    pub fn started(&self) -> bool {
        self.started
    }

    pub fn turns(&self) -> u32 {
        self.turns
    }

    pub fn exec(&self) -> Duration {
        self.exec
    }

    pub fn verdict(&self, time_left: Duration) -> Verdict {
        if self.started
            && (self.max_turns.is_some_and(|m| self.turns >= m)
                || self.max_exec.is_some_and(|m| self.exec >= m))
        {
            return Verdict::ContinuationLimit;
        }
        if time_left < self.min_left {
            return Verdict::TooLate;
        }
        Verdict::Allowed
    }

    /// A message-driven turn starts. Only turns that actually start count, and only after the allowance has started. Returns whether
    /// this one counts.
    pub fn turn_started(&mut self) -> bool {
        self.in_flight = self.started;
        if self.started {
            self.turns += 1;
        }
        self.started
    }

    /// The message-driven turn ended after `elapsed`. Its time counts when it started inside the allowance.
    pub fn turn_ended(&mut self, elapsed: Duration) {
        if self.in_flight {
            self.exec += elapsed;
        }
        self.in_flight = false;
    }
}
