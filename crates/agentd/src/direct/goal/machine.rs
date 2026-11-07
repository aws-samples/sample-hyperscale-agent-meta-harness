//! The rules of the goal-loop supervisor, pure: what ends a run, when a "continue" is sent, when a done claim counts.
//! It reads facts (a round started, a round ended and what the tree looked like, the model called the completion tool, the goal is or is not
//! in the state file) with the time they were seen, and answers [`Decision`]. It has no clock, no file and no engine of its own, so the tests
//! move time by hand.
//!
//! There is no cap on rounds or tokens: every brake is a stall. The numbers are [`Limits`].
//!
//! Order of the answer: a done claim, then BLOCKED twice, then a text-only stall, then rounds that change nothing, then a round in flight
//! (wait), then a goal that left the state file, then a dead loop (continue, or end the run after too many continues with nothing changed).
//! A brake that fired stays the answer.

use crate::direct::goal::rounds::{RoundEnd, blocked_line};
use std::time::{Duration, Instant};

/// The thresholds of the rules. `Default` holds the numbers of the decision record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How the words of a brake name the engine (its display name, or `the engine`).
    pub who: &'static str,
    /// Rounds in a row that end with the same `BLOCKED:` line.
    pub blocked_rounds: u32,
    /// Rounds in a row with no tool call and no change of the tree.
    pub stall_rounds: u32,
    /// Rounds in a row with no change of the tree, whatever they did.
    pub idle_rounds: u32,
    /// How long after a completed round with no new round the loop counts as dead.
    pub kick_after: Duration,
    /// How long after a round that ended aborted or failed (the plugin queues nothing after those) the loop counts as dead.
    pub dead_grace: Duration,
    /// Continues allowed with no change of the tree between them.
    pub kicks_max: u32,
    /// How long a done claim waits for a new round that would take its place.
    pub settle: Duration,
    /// Without the observer: how long the engine must be quiet after a done claim.
    pub fallback_quiet: Duration,
    /// How long the goal must be missing from the state file before that is reported (a read can meet half a write).
    pub cleared_confirm: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            who: "the engine",
            blocked_rounds: 2,
            stall_rounds: 3,
            idle_rounds: 5,
            kick_after: Duration::from_secs(15),
            dead_grace: Duration::from_secs(2),
            kicks_max: 2,
            settle: Duration::from_secs(3),
            fallback_quiet: Duration::from_secs(15),
            cleared_confirm: Duration::from_secs(2),
        }
    }
}

/// What the work tree looked like at the end of a round, compared with the end of the round before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fp {
    /// It could not be measured: counted as a change, never as "nothing changed".
    Unknown,
    /// The same as at the end of the round before.
    Same,
    /// This fingerprint (compared with the last one known).
    Is(String),
}

/// A fact the supervisor is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obs {
    /// An engine run started (a line of the rounds log).
    Start { n: u64 },
    /// An engine run ended.
    End { end: RoundEnd, fp: Fp },
    /// The model called the completion tool and the plugin accepted it (the ACP stream says so).
    Candidate { summary: Option<String> },
    /// The engine sent a line (the observer may be missing; this is the only clock then).
    Line,
}

/// The rule that ended a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// The same `BLOCKED:` line in a row.
    Blocked,
    /// Rounds with no tool call and no change.
    TextOnly,
    /// Rounds with no change.
    NoProgress,
    /// The loop stopped again and again and nothing changed.
    DeadLoop,
    /// The model called the completion tool but its round did not end with a message.
    Unfinished,
    /// The goal left the state file and the model never completed it.
    Cleared,
    /// Tool calls inside one round that changed no file and brought no text (many varied calls over a long time); see [`watch`](super::watch).
    RoundNoProgress,
}

impl Rule {
    /// A short word for events and logs.
    pub fn word(self) -> &'static str {
        match self {
            Self::Blocked => "blocked-twice",
            Self::TextOnly => "text-only-stall",
            Self::NoProgress => "no-progress",
            Self::DeadLoop => "dead-loop",
            Self::Unfinished => "unfinished-claim",
            Self::Cleared => "goal-cleared",
            Self::RoundNoProgress => "round-no-progress",
        }
    }
}

/// An end of the run chosen by the harness, with the words that say so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brake {
    pub rule: Rule,
    /// One line that begins "The harness ended the run:" and names the rule; a quoted line of the engine stays in quotes.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do yet.
    Wait,
    /// The loop ended with a done claim that counts.
    Done { summary: Option<String> },
    /// End the run.
    Brake(Brake),
    /// The loop is dead: send "continue" ([`Machine::kicked`] after it was sent).
    Kick,
}

#[derive(Clone, Debug)]
struct Kicked {
    at: Instant,
    /// The number of rounds started when the continue was sent: a new start means the continue (or something else) started a round.
    starts: u64,
}

/// The state of one goal loop.
#[derive(Debug)]
pub struct Machine {
    limits: Limits,
    last_fp: Option<String>,
    open: Option<u64>,
    starts: u64,
    ends: u64,
    last_end: Option<(RoundEnd, Instant)>,
    blocked: Option<(String, String, u32)>,
    text_only: u32,
    idle: u32,
    kicks: u32,
    kicked: Option<Kicked>,
    candidate: Option<(Instant, Option<String>)>,
    last_line: Option<Instant>,
    /// The goal was last read as active (`true`) or missing since the given time.
    goal_missing_since: Option<Instant>,
    brake: Option<Brake>,
}

impl Machine {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            last_fp: None,
            open: None,
            starts: 0,
            ends: 0,
            last_end: None,
            blocked: None,
            text_only: 0,
            idle: 0,
            kicks: 0,
            kicked: None,
            candidate: None,
            last_line: None,
            goal_missing_since: None,
            brake: None,
        }
    }

    /// The fingerprint of the tree when the loop began (the first round is compared with it).
    pub fn with_base(mut self, fp: Option<String>) -> Self {
        self.last_fp = fp;
        self
    }

    pub fn open_round(&self) -> bool {
        self.open.is_some()
    }

    /// How many rounds have ended.
    pub fn ends(&self) -> u64 {
        self.ends
    }

    pub fn starts(&self) -> u64 {
        self.starts
    }

    pub fn last_end(&self) -> Option<&RoundEnd> {
        self.last_end.as_ref().map(|(e, _)| e)
    }

    /// When the last round was seen to end.
    pub fn last_end_at(&self) -> Option<Instant> {
        self.last_end.as_ref().map(|(_, at)| *at)
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Whether the rounds log has shown anything yet.
    pub fn observed(&self) -> bool {
        self.starts > 0 || self.ends > 0
    }

    pub fn has_candidate(&self) -> bool {
        self.candidate.is_some()
    }

    /// The goal is in the state file (`true`) or is not (`false`), as read at `at`.
    pub fn goal_seen(&mut self, at: Instant, active: bool) {
        if active {
            self.goal_missing_since = None;
        } else if self.goal_missing_since.is_none() {
            self.goal_missing_since = Some(at);
        }
    }

    /// A continue was sent at `at`.
    pub fn kicked(&mut self, at: Instant) {
        self.kicks += 1;
        self.kicked = Some(Kicked {
            at,
            starts: self.starts,
        });
    }

    pub fn observe(&mut self, at: Instant, obs: Obs) {
        match obs {
            Obs::Line => self.last_line = Some(at),
            Obs::Start { n } => {
                self.starts += 1;
                self.open = Some(n);
                if self
                    .kicked
                    .as_ref()
                    .is_some_and(|k| k.starts != self.starts)
                {
                    self.kicked = None;
                }
            }
            Obs::Candidate { summary } => {
                if self.candidate.is_none() {
                    self.candidate = Some((at, summary));
                }
            }
            Obs::End { end, fp } => self.round_ended(at, end, fp),
        }
    }

    fn round_ended(&mut self, at: Instant, end: RoundEnd, fp: Fp) {
        self.ends += 1;
        self.open = None;
        let progress = match &fp {
            Fp::Unknown => true,
            Fp::Same => false,
            Fp::Is(f) => self.last_fp.as_deref() != Some(f.as_str()),
        };
        if let Fp::Is(f) = fp {
            self.last_fp = Some(f);
        }
        if progress {
            self.text_only = 0;
            self.idle = 0;
            self.kicks = 0;
        }
        if end.status == "completed" {
            match blocked_line(&end.text) {
                Some(b) => {
                    let n = match &self.blocked {
                        Some((key, _, n)) if *key == b.key => n + 1,
                        _ => 1,
                    };
                    self.blocked = Some((b.key, b.shown, n));
                }
                None => self.blocked = None,
            }
            if progress {
                // counters were reset above
            } else {
                self.idle += 1;
                self.text_only = if end.iterations <= 1 {
                    self.text_only + 1
                } else {
                    0
                };
            }
        }
        self.last_end = Some((end, at));
    }

    fn take_brake(&mut self, rule: Rule, detail: String) -> Decision {
        let b = Brake { rule, detail };
        self.brake = Some(b.clone());
        Decision::Brake(b)
    }

    pub fn decide(&mut self, at: Instant) -> Decision {
        if let Some(b) = &self.brake {
            return Decision::Brake(b.clone());
        }
        if let Some((claimed_at, summary)) = self.candidate.clone() {
            return self.judge_claim(at, claimed_at, summary);
        }
        if let Some((_, shown, n)) = &self.blocked
            && *n >= self.limits.blocked_rounds
        {
            let detail = format!(
                "The harness ended the run: {} ended {n} rounds in a row with the same last line. Its words: \"{shown}\"",
                self.limits.who
            );
            return self.take_brake(Rule::Blocked, detail);
        }
        if self.text_only >= self.limits.stall_rounds {
            let detail = format!(
                "The harness ended the run: {} rounds in a row had no tool call and changed no file.",
                self.text_only
            );
            return self.take_brake(Rule::TextOnly, detail);
        }
        if self.idle >= self.limits.idle_rounds {
            let detail = format!(
                "The harness ended the run: {} rounds in a row changed no file.",
                self.idle
            );
            return self.take_brake(Rule::NoProgress, detail);
        }
        if self.open.is_some() {
            return Decision::Wait;
        }
        if let Some(since) = self.goal_missing_since
            && at.saturating_duration_since(since) >= self.limits.cleared_confirm
        {
            return self.take_brake(
                Rule::Cleared,
                format!(
                    "The harness ended the run: the goal is no longer in the state file and {} never completed it.",
                    self.limits.who
                ),
            );
        }
        self.dead_loop(at)
    }

    fn judge_claim(
        &mut self,
        at: Instant,
        claimed_at: Instant,
        summary: Option<String>,
    ) -> Decision {
        if self.open.is_some() {
            return Decision::Wait;
        }
        let gone = self
            .goal_missing_since
            .is_some_and(|s| at.saturating_duration_since(s) >= self.limits.cleared_confirm);
        let Some((end, ended_at)) = self.last_end.clone() else {
            // No observer: only the engine's quiet and the missing goal are left to go on.
            let quiet_since = self.last_line.map_or(claimed_at, |l| l.max(claimed_at));
            if gone && at.saturating_duration_since(quiet_since) >= self.limits.fallback_quiet {
                return Decision::Done { summary };
            }
            return Decision::Wait;
        };
        if end.status != "completed" {
            let detail = format!(
                "The harness ended the run: {} called the goal-complete tool, but its round ended {}.",
                self.limits.who, end.status
            );
            return self.take_brake(Rule::Unfinished, detail);
        }
        if end.text.trim().is_empty() {
            return self.take_brake(
                Rule::Unfinished,
                format!(
                    "The harness ended the run: {} called the goal-complete tool, but its round ended with no message.",
                    self.limits.who
                ),
            );
        }
        if at.saturating_duration_since(ended_at) >= self.limits.settle {
            Decision::Done { summary }
        } else {
            Decision::Wait
        }
    }

    fn dead_loop(&mut self, at: Instant) -> Decision {
        let goal_active = self.goal_missing_since.is_none();
        let Some((end, ended_at)) = self.last_end.clone() else {
            return Decision::Wait;
        };
        if !goal_active {
            return Decision::Wait;
        }
        // After a continue the loop gets the long time again, whatever the last round said.
        let (since, wait) = match &self.kicked {
            Some(k) => (k.at.max(ended_at), self.limits.kick_after),
            None if end.status == "completed" => (ended_at, self.limits.kick_after),
            None => (ended_at, self.limits.dead_grace),
        };
        if at.saturating_duration_since(since) < wait {
            return Decision::Wait;
        }
        if self.kicks >= self.limits.kicks_max {
            let detail = format!(
                "The harness ended the run: the goal loop stopped {} times in a row with no change to the files in between (the last round ended {}).",
                self.kicks, end.status
            );
            return self.take_brake(Rule::DeadLoop, detail);
        }
        Decision::Kick
    }
}
