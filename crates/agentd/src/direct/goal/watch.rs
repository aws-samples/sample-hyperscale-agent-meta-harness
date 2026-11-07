//! The brake inside one round. The round rules of [`machine`](super::machine) judge a round when it ends;
//! a run that makes tool call after tool call inside one long round, each call different, changes no file and writes no text, has no end
//! to judge (the engine's own loop guard only sees identical calls). The watch counts the tool calls and
//! is told now and then what the tree looks like. Two limits, both stalls and neither a cap on work:
//!
//! - `calls` tool calls in a row with an unchanged tree and no new text from the model (a new text or a change of the tree starts the
//!   count again);
//! - `window` of wall-clock time since the tree last changed, with at least `min_calls` tool calls in it (a build or a test run that
//!   takes the whole window is one or two calls, not a poll).
//!
//! A third input: evidence that a command of the engine's user is running ([`Command`], sampled by the module every few seconds,
//! see [`command`](super::command)). A model that starts a long test run in the background can only wait for it with calls (the shell tool
//! gives up after ten minutes), and those polls are waiting, not polling. A call is taken back, and the time the command ran is left out of
//! the window, only when the sessions that used the CPU are older than the call by [`START_MARGIN`]: the model started that command with an
//! earlier call. A command a call starts and waits for in the foreground is as old as the call or younger, so a loop that runs the failing
//! test suite again and again stays counted and is ended at 30 calls or 20 minutes (the review of the first version). The history before a
//! command ran stays (paused, not forgiven). The command does not hold the run for ever: once a command has been seen running, a tree that
//! stays unchanged for the grace (two hours by default, never less than the window, counted from the last change of the tree) ends the run,
//! and `Unknown` evidence protects nothing. A CPU-bound job the model started earlier protects a later call that has nothing to do with it,
//! until the grace.
//!
//! The hold is told to the run's events: the first time a sample takes calls back or credits time the watch has a [`HoldNote::Held`] for
//! the module to take ([`Watch::take_note`]), and when no time was credited for the release time it has a [`HoldNote::Released`], so a person
//! reading the events sees when the brake was held back, for how long and why it counts again. One note for each change.
//!
//! A fourth input (the credit for new information): a model that reads and plans before it edits makes no file change for as long as the task
//! is large, and a run that reads the code of a feature that touches several modules can make dozens of calls that each read something the run had not seen (every input new,
//! every answer new, none failed). A call whose input is new to the run, whose answer brings at least [`MIN_NEW_LINES`] lines that no earlier answer
//! of the run had, and that succeeded, is therefore not counted toward the two limits, up to an allowance ([`Watch::with_new_calls`]) between two
//! changes of the tree. A call that repeats an earlier input, one whose answer is the earlier answer again or differs from it in a line or two (a
//! poll that gets the same answer however its command is worded, or `date` as the only change), a failed call, a call with no answer on the wire
//! and every call beyond the allowance are counted as before, so a loop of identical calls, a poll of an idle session and a run that keeps failing
//! end as they did. What a call asked and what came back are read from the wire by [`input_key`] and [`output_key`]; an engine that sends neither
//! gets no credit. An allowance of 0 gives the old rule back.
//!
//! A fifth input: a call of the engine's helper-agent tool that has not been answered yet is a wait for delegated
//! work. The lifecycle review of a run starts several read-only reviewers, the engine waits for them, they cannot change the tree, and
//! their calls arrive on the same stream as the engine's own with nothing that names the agent. Runs of a release gate were ended 18 to 21
//! minutes into that review, with the code complete and the reviewers' findings not yet back. While at least one
//! helper call is open the time is protected (paused, like a running command, once for the union of the open calls) and the calls that arrive
//! are not counted toward either limit; the answers of those calls are not remembered as seen, because the engine's own agent did not read them.
//! The hold is bounded without naming any agent: the grace (counted from the last change of the tree, from the wall clock, so a lost
//! completion update or a silent stream cannot extend it) and a repetition limit: an input may arrive once per helper begun since the last
//! change of the tree plus one (every reviewer reads the same files), and as many calls beyond that as the `calls` limit end the run. The
//! helper's time is not forgiven: when the last helper call is answered the clock goes on from the time that was not protected before.
//!
//! A tree that cannot be read is never read as "nothing changed". Pure: no clock of its own, no file.

use crate::direct::goal::machine::{Brake, Rule};
use harness_protocol::direct::{GOAL_COMMAND_GRACE_S, HARNESS_STOP_PREFIX};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

/// What a sample of the engine user's processes found over the time since the previous sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// The processes could not be read, or there is no earlier sample to compare with: no evidence, so nothing is protected.
    Unknown,
    /// Nothing but the engine and what was there when the session began was using CPU.
    Idle,
    /// Processes that were not there when the session began used CPU: a command is running. `age` is how old, at the sample, the youngest of
    /// the sessions is that are needed to reach the rate: the sessions at least that old used enough CPU together.
    Active { age: Duration },
}

/// How much older than a call the sessions that use the CPU must be for the call to be a wait for them. A command the call itself starts is
/// younger than the call; the margin covers the time between the engine starting a command and agentd reading the line that announced the
/// call (a busy host), and keeps a poll that follows the start of a job within seconds counted: the next one is not.
pub const START_MARGIN: Duration = Duration::from_secs(5);

/// The most calls the watch keeps the arrival of while it waits for a sample. A call beyond it stays counted, as if its evidence were `Idle`.
pub const UNFOLDED_MAX: usize = 1024;

/// How long without any time credited ends a hold in the events: the hold is noted as released when no sample took a call back or credited time
/// for this long. Long enough that a sample or two without credit (a quiet moment of the job) do not make a note.
pub const RELEASE_AFTER: Duration = Duration::from_secs(30);

/// What the watch tells the run's events about the hold on the brake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldNote {
    /// The brake is held back from now: a command the model started `age` ago has used the CPU lately (in the last sample or over the window).
    Held { age: Duration },
    /// No time was credited for the release time: the brake counts the calls again. `held` is the time credited during the hold.
    Released { held: Duration },
    /// A stretch of the unchanged tree was held back because the session's CPU outside agentd and the engine was busy at `pct` percent of one CPU
    /// over `span`.
    Busy { pct: u32, span: Duration },
}

/// The shortest stretch of readings the CPU is judged over: a burst of calls shorter than this is never excused by it.
pub const BUSY_MIN_SPAN: Duration = Duration::from_secs(30);

/// Below this share of one CPU, in percent, the session's CPU is called idle in a stop's words; above it, busy at that share.
pub const IDLE_BELOW_PCT: u32 = 5;

/// The most inputs the watch remembers to tell a new call from an old one. Beyond it nothing is new any more: the credit stops and the calls are
/// counted as they were before it.
pub const SEEN_MAX: usize = 20_000;

/// The most distinct lines of answers the watch remembers, as hashes. Beyond it no line is new.
pub const SEEN_LINES_MAX: usize = 200_000;

/// The fewest lines an answer must bring that no earlier answer of the run had for its call to count as bringing something new. Reading code
/// brings dozens; a poll that prints the time, a process list or a counter changes a line or two.
pub const MIN_NEW_LINES: usize = 3;

/// The most distinct lines of one answer that are read; a longer answer is read as far as this.
const MAX_LINES: usize = 5_000;

/// The most CPU readings the watch keeps (a reading every few seconds over the window is a few hundred).
const MAX_CPU_LOG: usize = 20_000;

/// The longest time one sample is credited for. The module samples every few seconds while calls are pending; a longer gap between two samples
/// (no call was pending, or the host stalled) is not evidence that a command ran through all of it.
pub const MAX_CREDIT: Duration = Duration::from_secs(60);

/// The two limits of the watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchLimits {
    /// Tool calls in a row with an unchanged tree and no new text.
    pub calls: u32,
    /// Wall-clock time with an unchanged tree.
    pub window: Duration,
    /// The least number of tool calls inside the window for the window to count.
    pub min_calls: u32,
    /// Who made the calls, in the words of the detail (the engine's label).
    pub who: &'static str,
}

#[derive(Clone, Debug)]
pub struct Watch {
    limits: WatchLimits,
    /// Tool calls since the last new text, the last change of the tree or the last boundary.
    run_calls: u32,
    /// Tool calls since the last change of the tree or the last boundary, text or not.
    calls_since_change: u32,
    /// Every tool call the watch was told about.
    total_calls: u64,
    /// The time of the last change of the tree, or of the last boundary.
    since: Instant,
    /// The fingerprint of the tree at the last look that read it.
    fp: Option<String>,
    looked: Option<Instant>,
    calls_at_look: u64,
    /// How long the tree may stay unchanged once a command was seen running ([`Watch::with_command_grace`]).
    grace: Duration,
    /// The arrival of the tool calls since the last sample of the evidence, with the id of a call that may still earn a credit: the ones an
    /// `Active` sample may take back.
    unfolded: Vec<(Instant, Option<String>)>,
    /// The arrival of the newest tool call, kept across boundaries: a long poll is still the call that waits for a job.
    last_call: Option<Instant>,
    /// The time, since the last change of the tree or the last boundary, in which a command was seen running.
    protected: Duration,
    /// The time of the last sample of the evidence, or of the last boundary.
    last_eval: Instant,
    /// How long without credit ends a hold in the events ([`Watch::with_release_after`]).
    release_after: Duration,
    /// A hold is going on: a sample took calls back or credited time and no `release_after` has passed since.
    holding: bool,
    /// The time credited since the hold began.
    hold_credit: Duration,
    /// The time of the last sample that held the brake back.
    last_credit: Option<Instant>,
    /// The change of the hold that the module has not taken yet.
    note: Option<HoldNote>,
    /// The share of one CPU, in percent, the session must have used outside agentd and the engine over a stretch for it to count as
    /// progress; 0: the signal is off.
    busy_pct: u32,
    /// The shortest stretch the CPU is judged over.
    busy_min_span: Duration,
    /// The readings of the session's CPU: the time and a counter of clock ticks that never goes down.
    cpu_log: VecDeque<(Instant, u64)>,
    /// The time of the last change of the tree or boundary. A busy stretch moves `since` on; the grace of a running command counts from here.
    anchor: Instant,
    /// A stretch of this unchanged tree was held back for the session's CPU: the grace applies as it does for a running command.
    busy_seen: bool,
    /// The most calls between two changes of the tree that bring something new and are not counted (the allowance); 0: no call is credited.
    new_budget: u32,
    /// The calls credited since the last change of the tree or boundary.
    new_used: u32,
    /// What the calls of the run asked ([`input_key`]) and the lines of what came back ([`output_key`]), for the whole run.
    seen_in: HashSet<u64>,
    seen_lines: HashSet<u64>,
    /// The calls whose input was new and that have no answer yet, by the id the engine gave them.
    open: HashMap<String, Open>,
    /// How many times the model wrote text: a call that began before a text no longer counts in the calls since that text.
    text_gen: u64,
    /// The helper-agent calls that are open, by the id the engine gave them. A set, not a counter: a call that is announced twice or
    /// answered twice leaves it right.
    delegates: HashSet<String>,
    /// The time up to which the open helper calls are accounted; `Some` exactly while `delegates` is not empty.
    deleg_mark: Option<Instant>,
    /// The time, since the last change of the tree or boundary, in which a helper call was open (part of `protected`).
    helper_held: Duration,
    /// Helper calls begun since the last change of the tree or boundary.
    hold_spawned: u32,
    /// How many times each input arrived while a helper call was open, since the last change of the tree or boundary.
    hold_inputs: HashMap<u64, u32>,
    /// Calls beyond the repetition limit of the hold.
    hold_excess: u32,
    /// Calls that arrived while a helper call was open, since the last change of the tree or boundary.
    hold_calls: u32,
    /// The sentence a stop ends with: what the user can run next. Given by the module, which knows the run.
    next_step: Option<String>,
}

/// A call that may earn a credit when its answer comes.
#[derive(Clone, Debug)]
struct Open {
    text_gen: u64,
}

impl Watch {
    /// `base` is the fingerprint of the tree when the watch begins, if it could be read.
    pub fn new(limits: WatchLimits, now: Instant, base: Option<String>) -> Self {
        Self {
            limits,
            run_calls: 0,
            calls_since_change: 0,
            total_calls: 0,
            since: now,
            fp: base,
            looked: None,
            calls_at_look: 0,
            grace: Duration::from_secs(u64::from(GOAL_COMMAND_GRACE_S.default)),
            unfolded: Vec::new(),
            last_call: None,
            protected: Duration::ZERO,
            last_eval: now,
            release_after: RELEASE_AFTER,
            holding: false,
            hold_credit: Duration::ZERO,
            last_credit: None,
            note: None,
            busy_pct: 0,
            busy_min_span: BUSY_MIN_SPAN,
            cpu_log: VecDeque::new(),
            anchor: now,
            busy_seen: false,
            new_budget: 0,
            new_used: 0,
            seen_in: HashSet::new(),
            seen_lines: HashSet::new(),
            open: HashMap::new(),
            text_gen: 0,
            delegates: HashSet::new(),
            deleg_mark: None,
            helper_held: Duration::ZERO,
            hold_spawned: 0,
            hold_inputs: HashMap::new(),
            hold_excess: 0,
            hold_calls: 0,
            next_step: None,
        }
    }

    /// The sentence every stop of the watch ends with, for the user: what to run next.
    pub fn with_next_step(mut self, text: String) -> Self {
        self.next_step = Some(text);
        self
    }

    /// The share of one CPU, in percent, that the session outside agentd and the engine must have used over a stretch for the stretch to count as
    /// progress; 0 switches the signal off.
    pub fn with_busy_pct(mut self, pct: u32) -> Self {
        self.busy_pct = pct;
        self
    }

    /// The shortest stretch the CPU is judged over (default [`BUSY_MIN_SPAN`]); a shorter one is never excused by it.
    pub fn with_busy_min_span(mut self, span: Duration) -> Self {
        self.busy_min_span = span;
        self
    }

    /// A reading of the CPU the session used outside agentd and the engine: `ticks` is a counter of clock ticks (100 a second) that never goes
    /// back; `None` when it could not be read.
    pub fn cpu(&mut self, now: Instant, ticks: Option<u64>) {
        let Some(ticks) = ticks.filter(|_| self.busy_pct > 0) else {
            return;
        };
        // a counter that went back is read as the same
        let ticks = self
            .cpu_log
            .back()
            .map_or(ticks, |(_, last)| ticks.max(*last));
        self.cpu_log.push_back((now, ticks));
        // One reading at least a window old is kept as the start of the longest stretch; the ones before it are of no use.
        let window = self.limits.window;
        while self.cpu_log.len() > 2
            && self
                .cpu_log
                .get(1)
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= window)
        {
            self.cpu_log.pop_front();
        }
        while self.cpu_log.len() > MAX_CPU_LOG {
            self.cpu_log.pop_front();
        }
    }

    /// What the session's CPU did over the last `want`, judged from the readings: the newest reading and the newest one at least `want` older
    /// (the oldest there is when the log is younger than that). Unknown when the signal is off, nothing was read, or the stretch the readings
    /// cover is shorter than the least stretch.
    fn cpu_view(&self, want: Duration) -> CpuView {
        if self.busy_pct == 0 {
            return CpuView::Off;
        }
        let Some(&(t1, c1)) = self.cpu_log.back() else {
            return CpuView::Unknown;
        };
        let start = self
            .cpu_log
            .iter()
            .rev()
            .find(|(t, _)| t1.saturating_duration_since(*t) >= want)
            .or_else(|| self.cpu_log.front())
            .copied();
        let Some((t0, c0)) = start else {
            return CpuView::Unknown;
        };
        let span = t1.saturating_duration_since(t0);
        if span < self.busy_min_span {
            return CpuView::Unknown;
        }
        // 100 ticks a second are one whole CPU, so ticks a second are percent of one CPU.
        let pct = u32::try_from(u128::from(c1.saturating_sub(c0)) * 1000 / span.as_millis().max(1))
            .unwrap_or(u32::MAX);
        if pct >= self.busy_pct {
            CpuView::Progress { pct, span }
        } else if pct < IDLE_BELOW_PCT {
            CpuView::Idle
        } else {
            CpuView::Busy(pct)
        }
    }

    /// A stretch of the unchanged tree was busy enough to count as progress: the counts start again, the grace of a running command does not.
    fn forgive(&mut self, now: Instant, pct: u32, span: Duration) {
        self.run_calls = 0;
        self.calls_since_change = 0;
        self.since = now;
        self.unfolded.clear();
        self.open.clear();
        self.protected = Duration::ZERO;
        self.last_eval = now;
        self.busy_seen = true;
        self.note = Some(HoldNote::Busy { pct, span });
    }

    /// How long without credit ends a hold in the events (default [`RELEASE_AFTER`]).
    pub fn with_release_after(mut self, release_after: Duration) -> Self {
        self.release_after = release_after;
        self
    }

    /// The change of the hold on the brake since the last call, for the run's events: `Held` when a sample first took a call back or credited
    /// time, `Released` when none did for the release time.
    pub fn take_note(&mut self) -> Option<HoldNote> {
        self.note.take()
    }

    /// The allowance of calls that bring something new: between two changes of the tree this many calls whose input is new to the run, whose answer
    /// has at least [`MIN_NEW_LINES`] lines no earlier answer had, and that succeeded, are not counted ([`Watch::tool_call_keyed`], [`Watch::tool_done`]). 0 counts every call, as before.
    pub fn with_new_calls(mut self, budget: u32) -> Self {
        self.new_budget = budget;
        self
    }

    /// The grace: how long the tree may stay unchanged while a command is seen running. Never less than the window.
    pub fn with_command_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    fn grace_effective(&self) -> Duration {
        self.grace.max(self.limits.window)
    }

    /// The engine began a tool call (not the call of the completion tool: that is a done claim). A call counted this way can earn no credit: use
    /// [`Watch::tool_call_keyed`] when the wire names the call and what it asked.
    pub fn tool_call(&mut self, now: Instant) {
        self.count_call(now, None);
    }

    fn count_call(&mut self, now: Instant, id: Option<&str>) {
        self.account(now);
        if !self.delegates.is_empty() {
            // A call that arrives while a helper agent is working is the helper's, as far as the stream shows: it is seen, never counted.
            self.total_calls += 1;
            self.last_call = Some(now);
            self.hold_calls = self.hold_calls.saturating_add(1);
            return;
        }
        self.total_calls += 1;
        self.run_calls = self.run_calls.saturating_add(1);
        self.calls_since_change = self.calls_since_change.saturating_add(1);
        if self.unfolded.len() < UNFOLDED_MAX {
            self.unfolded.push((now, id.map(str::to_owned)));
        }
        self.last_call = Some(now);
    }

    /// The engine began the tool call `id`, whose input has the key `input` ([`input_key`]). The call is counted at once, like any call; when its
    /// input is new to the run it may be given back when the answer comes ([`Watch::tool_done`]).
    pub fn tool_call_keyed(&mut self, now: Instant, id: &str, input: u64) {
        self.count_call(now, Some(id));
        if !self.delegates.is_empty() {
            // In a hold an input may arrive once for each helper begun, and once for the engine's own agent; the calls beyond that are
            // the repetition the hold is bounded by.
            if self.hold_inputs.len() < SEEN_MAX || self.hold_inputs.contains_key(&input) {
                let n = self.hold_inputs.entry(input).or_insert(0);
                *n = n.saturating_add(1);
                if *n > self.hold_spawned.saturating_add(1) {
                    self.hold_excess = self.hold_excess.saturating_add(1);
                }
            }
            return;
        }
        if self.new_budget > 0
            && self.open.len() < UNFOLDED_MAX
            && self.seen_in.len() < SEEN_MAX
            && self.seen_in.insert(input)
        {
            self.open.insert(
                id.to_owned(),
                Open {
                    text_gen: self.text_gen,
                },
            );
        }
    }

    /// The tool call `id` ended. `answer` is what came back ([`output_key`]; `None` when the update carried no answer). A call whose input was
    /// new, that succeeded, whose answer brings at least [`MIN_NEW_LINES`] lines no earlier answer of the run had and that began in this stretch of
    /// the unchanged tree is given back while the allowance lasts: it is taken out of both counts and out of the calls a running command may take
    /// back, and it uses one of the allowance. Every other end changes no count. The lines of every answer are remembered, credited or not.
    pub fn tool_done(&mut self, id: &str, answer: Option<&Answer>) {
        let open = self.open.remove(id);
        if !self.delegates.is_empty() {
            // The answer of a call in a hold is the helper's reading: the engine's own agent did not see it, so it is not remembered as seen.
            return;
        }
        let Some(answer) = answer else { return };
        if self.new_budget == 0 {
            return;
        }
        let mut fresh = 0usize;
        for line in &answer.lines {
            if self.seen_lines.len() < SEEN_LINES_MAX && self.seen_lines.insert(*line) {
                fresh += 1;
            }
        }
        let Some(open) = open else { return };
        if !answer.ok || fresh < MIN_NEW_LINES || self.new_used >= self.new_budget {
            return;
        }
        self.new_used += 1;
        if open.text_gen == self.text_gen {
            self.run_calls = self.run_calls.saturating_sub(1);
        }
        self.calls_since_change = self.calls_since_change.saturating_sub(1);
        if let Some(at) = self
            .unfolded
            .iter()
            .position(|(_, i)| i.as_deref() == Some(id))
        {
            self.unfolded.remove(at);
        }
    }

    /// The calls given back since the last change of the tree or boundary: the part of the allowance that is used.
    pub fn new_credits(&self) -> u32 {
        self.new_used
    }

    /// The sentences after the first one of a stop. The first sentence is the reason the run's label carries (at most 120 characters). The
    /// detail as a whole is cut at 300 characters (`MAX_DIRECT_DETAIL_CHARS`), so what the user can run next comes before the facts that explain
    /// the count: a cut then loses a fact and never the command.
    fn tail_sentences(&self, view: CpuView) -> String {
        let mut out: Vec<String> = Vec::new();
        out.extend(self.next_step.clone());
        out.extend(view.sentence());
        if !self.helper_held.is_zero() {
            out.push(format!(
                "Helper agents were open {}; {} calls not counted.",
                approx(self.helper_held),
                self.hold_calls
            ));
        }
        out.join(" ")
    }

    /// A stop of the watch: the first sentence, which the label carries, and the rest.
    fn stop(&self, first: &str, view: CpuView) -> Brake {
        let tail = self.tail_sentences(view);
        let detail = if tail.is_empty() {
            format!("{HARNESS_STOP_PREFIX} {first}.")
        } else {
            format!("{HARNESS_STOP_PREFIX} {first}. {tail}")
        };
        Brake {
            rule: Rule::RoundNoProgress,
            detail,
        }
    }

    /// Accounts the time the open helper calls have been open since the last accounting: it is protected once, whatever the number of calls.
    fn account(&mut self, now: Instant) {
        if self.delegates.is_empty() {
            return;
        }
        if let Some(mark) = self.deleg_mark {
            let spent = now.saturating_duration_since(mark);
            self.protected += spent;
            self.helper_held += spent;
        }
        self.deleg_mark = Some(now);
    }

    /// The engine began a call of its helper-agent tool `id`. The call is a wait, not a poll: it is not counted, and the time until it is
    /// answered is protected. Several calls at once are one stretch.
    pub fn delegate_call(&mut self, now: Instant, id: &str) {
        self.account(now);
        self.total_calls += 1;
        self.last_call = Some(now);
        if self.delegates.insert(id.to_owned()) {
            self.hold_spawned = self.hold_spawned.saturating_add(1);
        }
        self.deleg_mark.get_or_insert(now);
    }

    /// The helper-agent call `id` was answered or failed. An answer to a call the watch does not know changes nothing; a repeated answer is harmless.
    pub fn delegate_done(&mut self, now: Instant, id: &str) {
        self.account(now);
        self.delegates.remove(id);
        if self.delegates.is_empty() {
            self.deleg_mark = None;
        }
    }

    /// How many helper-agent calls are open.
    pub fn delegates_open(&self) -> usize {
        self.delegates.len()
    }

    /// The model wrote new text.
    pub fn text(&mut self) {
        self.run_calls = 0;
        self.text_gen += 1;
    }

    /// A round began or ended, or a fresh session began: the counts start again, and no helper call is open any more (a new session has none).
    pub fn boundary(&mut self, now: Instant) {
        self.delegates.clear();
        self.deleg_mark = None;
        self.restart(now);
    }

    /// The tree changed or could not be read: the counts start again. A helper call that is open stays open, and the hold goes on from now.
    fn restart(&mut self, now: Instant) {
        self.deleg_mark = (!self.delegates.is_empty()).then_some(now);
        self.helper_held = Duration::ZERO;
        self.hold_spawned = 0;
        self.hold_inputs.clear();
        self.hold_excess = 0;
        self.hold_calls = 0;
        self.run_calls = 0;
        self.calls_since_change = 0;
        self.since = now;
        self.anchor = now;
        self.busy_seen = false;
        self.unfolded.clear();
        self.open.clear();
        self.new_used = 0;
        self.protected = Duration::ZERO;
        self.last_eval = now;
    }

    /// The evidence of a sample taken `now`, which covers the time since the previous sample (or the last boundary). An `Active` sample takes
    /// back the tool calls that came in that time and are younger than the sessions that used the CPU by [`START_MARGIN`] (a later call
    /// qualifies whenever an earlier one does), and credits the time (at most [`MAX_CREDIT`]) as protected when the newest call, which may be
    /// a poll that began long ago, qualifies. The other samples credit nothing and keep the calls counted.
    pub fn command(&mut self, now: Instant, evidence: Command) {
        let covered = now
            .saturating_duration_since(self.last_eval)
            .min(MAX_CREDIT);
        self.last_eval = now;
        let pending = std::mem::take(&mut self.unfolded);
        let Command::Active { age } = evidence else {
            self.hold(now, None, Duration::ZERO);
            return;
        };
        let waits = |arrived: Instant| age >= now.saturating_duration_since(arrived) + START_MARGIN;
        let mut credit = Duration::ZERO;
        let mut held = false;
        // While a helper call is open its hold protects the time already; a command that runs in it does not protect it twice.
        if self.delegates.is_empty() && self.last_call.is_some_and(waits) {
            self.protected += covered;
            credit = covered;
            held = true;
        }
        let mut taken = 0u32;
        for (arrived, id) in &pending {
            if waits(*arrived) {
                taken = taken.saturating_add(1);
                // a call taken back as a wait is not given back again when its answer comes
                if let Some(id) = id {
                    self.open.remove(id);
                }
            }
        }
        self.run_calls = self.run_calls.saturating_sub(taken);
        self.calls_since_change = self.calls_since_change.saturating_sub(taken);
        self.hold(now, (held || taken > 0).then_some(age), credit);
    }

    /// Follows the hold for the events: `held` is the age of the command when this sample took calls back or credited time.
    fn hold(&mut self, now: Instant, held: Option<Duration>, credit: Duration) {
        if let Some(age) = held {
            if !self.holding {
                self.holding = true;
                self.hold_credit = Duration::ZERO;
                self.note = Some(HoldNote::Held { age });
            }
            self.hold_credit += credit;
            self.last_credit = Some(now);
        } else if self.holding
            && self
                .last_credit
                .is_some_and(|t| now.saturating_duration_since(t) >= self.release_after)
        {
            self.holding = false;
            self.note = Some(HoldNote::Released {
                held: self.hold_credit,
            });
        }
    }

    /// The time in which a command was seen running since the last change of the tree or boundary.
    pub fn protected(&self) -> Duration {
        self.protected
    }

    /// Whether the evidence is worth sampling: a call is counted or waiting to be classified, or a command was seen running. A run that has
    /// made no call since the last change has nothing to protect.
    pub fn exposed(&self) -> bool {
        self.run_calls > 0
            || self.calls_since_change > 0
            || !self.unfolded.is_empty()
            || !self.protected.is_zero()
    }

    /// The grace has run out for a command that was seen running: the tree must be read to know whether the run ends.
    fn grace_due(&self, now: Instant) -> bool {
        (!self.protected.is_zero() || self.busy_seen || !self.delegates.is_empty())
            && now.saturating_duration_since(self.anchor) >= self.grace_effective()
    }

    /// Tool calls in a row since the last new text, change of the tree or boundary.
    pub fn calls_in_a_row(&self) -> u32 {
        self.run_calls
    }

    /// Whether the tree is worth reading now: a tool call came since the last look (or the grace of a running command is due) and the last
    /// look was `every` ago or more.
    pub fn wants_look(&self, now: Instant, every: Duration) -> bool {
        (self.total_calls > self.calls_at_look || self.grace_due(now))
            && self
                .looked
                .is_none_or(|at| now.saturating_duration_since(at) >= every)
    }

    /// The tree as it is `now` (`None`: it could not be read). A change starts the counts again; an unchanged tree may end the run.
    pub fn look(&mut self, now: Instant, fp: Option<String>) -> Option<Brake> {
        self.looked = Some(now);
        self.calls_at_look = self.total_calls;
        self.account(now);
        let Some(fp) = fp else {
            // Unknown is never "nothing changed".
            self.restart(now);
            return None;
        };
        if self.fp.as_deref() != Some(fp.as_str()) {
            self.fp = Some(fp);
            self.restart(now);
            return None;
        }
        let who = self.limits.who;
        if self.run_calls >= self.limits.calls {
            // The stretch is the time since the tree last changed (a window at most): 30 calls in a row can come well inside the window.
            let view = self.cpu_view(
                now.saturating_duration_since(self.since)
                    .min(self.limits.window),
            );
            if let CpuView::Progress { pct, span } = view {
                self.forgive(now, pct, span);
                return None;
            }
            return Some(self.stop(
                &format!(
                    "{who} made no progress in {} tool calls in a row: no file changed and no new text appeared",
                    self.run_calls
                ),
                view,
            ));
        }
        let elapsed = now.saturating_duration_since(self.since);
        if self.hold_excess >= self.limits.calls {
            // The helpers repeat the same calls beyond what independent readers of the same files explain: the hold does not excuse that.
            return Some(self.stop(
                &format!(
                    "{who} made no progress in {}: a helper agent kept repeating the same calls, and no file changed",
                    approx(elapsed)
                ),
                self.cpu_view(self.limits.window),
            ));
        }
        // The time a command or a helper agent was seen running is paused, not forgiven: it is left out of the wait and the rest stays.
        let waited = elapsed.saturating_sub(self.protected);
        if waited >= self.limits.window && self.calls_since_change >= self.limits.min_calls {
            let view = self.cpu_view(self.limits.window);
            if let CpuView::Progress { pct, span } = view {
                self.forgive(now, pct, span);
                return None;
            }
            let counted = self.calls_since_change;
            let total = counted.saturating_add(self.new_used);
            return Some(self.stop(
                &format!(
                    "{who} made no progress in {}: no file changed and {counted} of {total} tool calls counted as no progress",
                    span(self.limits.window)
                ),
                view,
            ));
        }
        if self.grace_due(now) {
            // The last backstop: a busy session, a running command or a helper agent is not excused for ever.
            let view = self.cpu_view(self.limits.window);
            let what = if self.delegates.is_empty() {
                "a command was running, but no file changed"
            } else {
                "a helper agent call stayed open, but no file changed"
            };
            return Some(self.stop(
                &format!(
                    "{who} made no progress in {}: {what}",
                    span(self.grace_effective())
                ),
                view,
            ));
        }
        None
    }
}

/// A time for a sentence of a person: whole minutes ("21 minutes"), or seconds under one minute.
fn approx(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{s} second{}", if s == 1 { "" } else { "s" });
    }
    let m = (s + 30) / 60;
    format!("{m} minute{}", if m == 1 { "" } else { "s" })
}

/// The names of the engine's tools the wire is read with.
#[derive(Clone, Copy, Debug)]
pub struct WireTools<'a> {
    /// The tool that completes a goal: a call of it is a done claim and no call to count.
    pub completion: &'a str,
    /// The tool that starts a helper agent, if the engine has one.
    pub delegate: Option<&'a str>,
}

/// Whether the title of a tool call names `tool`: `spawn_agent` or `spawn_agent: <task>`. A title that only starts with the same letters
/// (`spawn_agents`) is another tool.
pub fn calls_tool(title: &str, tool: &str) -> bool {
    title == tool
        || title
            .strip_prefix(tool)
            .is_some_and(|rest| rest.starts_with(':'))
}

fn call_id(update: &Value) -> Option<String> {
    match update.get("toolCallId") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Tells the watch what one `session/update` of the engine says (`u` is the update itself): a tool call (a helper-agent call opens a hold, any
/// other call is counted, and one whose input the wire names can earn a credit when its answer comes), the end of a call, and text of the model. The
/// module feeds the wire through it, and the replays of recorded runs call the same code.
pub fn feed_update(w: &mut Watch, now: Instant, u: &Value, tools: WireTools<'_>) {
    let status = u.get("status").and_then(Value::as_str);
    match u.get("sessionUpdate").and_then(Value::as_str) {
        Some("tool_call") => {
            let title = u.get("title").and_then(Value::as_str);
            let completion = title.is_some_and(|t| t.starts_with(tools.completion));
            let helper = title
                .zip(tools.delegate)
                .is_some_and(|(t, tool)| calls_tool(t, tool));
            match (call_id(u), helper) {
                (Some(id), true) => {
                    // A call of the helper-agent tool is a wait for delegated work, not a call to count. One that arrives already ended is no wait.
                    w.delegate_call(now, &id);
                    if matches!(status, Some("completed" | "failed")) {
                        w.delegate_done(now, &id);
                    }
                }
                _ if completion => {}
                // A call the wire names and says what it asked can earn a credit when its answer comes; any other is counted as it always was.
                (Some(id), false) => match input_key(u) {
                    Some(key) => w.tool_call_keyed(now, &id, key),
                    None => w.tool_call(now),
                },
                (None, _) => w.tool_call(now),
            }
        }
        Some("tool_call_update") => {
            let Some(id) = call_id(u) else { return };
            // Any status but the two that say the call goes on ends a helper-agent call.
            if !matches!(status, Some("pending" | "in_progress")) {
                w.delegate_done(now, &id);
            }
            match output_key(u) {
                Some(answer) => w.tool_done(&id, Some(&answer)),
                // an end with no answer on the wire: the call is closed and earns nothing
                None if matches!(status, Some("completed" | "failed")) => w.tool_done(&id, None),
                None => {}
            }
        }
        Some("agent_message_chunk") => {
            let text = u.pointer("/content/text").and_then(Value::as_str);
            if text.is_some_and(|t| !t.trim().is_empty()) {
                w.text();
            }
        }
        _ => {}
    }
}

/// What the session's CPU outside agentd and the engine did over a stretch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuView {
    /// The signal is off.
    Off,
    /// Nothing was read, or too short a stretch: no words about it.
    Unknown,
    /// Below [`IDLE_BELOW_PCT`] percent of one CPU.
    Idle,
    /// Some CPU, below the share that counts as progress.
    Busy(u32),
    /// At least the share that counts as progress.
    Progress { pct: u32, span: Duration },
}

impl CpuView {
    /// The sentence a stop carries: `CPU idle.`, `CPU busy at 30 percent.` (the session''s CPU outside the harness and the engine), or nothing when it is not known.
    fn sentence(self) -> Option<String> {
        match self {
            Self::Off | Self::Unknown => None,
            Self::Idle => Some("CPU idle.".to_string()),
            Self::Busy(pct) | Self::Progress { pct, .. } => {
                Some(format!("CPU busy at {pct} percent."))
            }
        }
    }
}

/// What a `tool_call` update asked for, as a key that is the same for the same ask whatever id the engine gave the call: a hash of the kind of
/// tool, the name of the tool (the title up to the first colon) and its input. A call that carries no input is keyed by its whole title. `None` when
/// the update names neither, so nothing can be compared.
pub fn input_key(update: &Value) -> Option<u64> {
    let kind = update.get("kind").and_then(Value::as_str).unwrap_or("");
    let title = update.get("title").and_then(Value::as_str);
    let input = update.get("rawInput").filter(|v| !v.is_null());
    let mut h = DefaultHasher::new();
    "in".hash(&mut h);
    kind.hash(&mut h);
    match (input, title) {
        (Some(input), title) => {
            title
                .and_then(|t| t.split(':').next())
                .unwrap_or("")
                .hash(&mut h);
            input.to_string().hash(&mut h);
        }
        (None, Some(title)) => title.hash(&mut h),
        (None, None) => return None,
    }
    Some(h.finish())
}

/// The name under which the engine's shell and file tools put the call's own input back into their answer (`[{"query": <the command>, "result":
/// <what it printed>, "success": true}]`). It is the call's words, not what the call brought, so the lines of an answer leave it out: a poll that
/// words its check differently each time and gets "missing" each time has one answer.
const ECHO_KEY: &str = "query";

/// What a `tool_call_update` that ended a call returned: the lines of the answer and whether the call succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The distinct non-blank lines of every string of the answer, as hashes (at most [`MAX_LINES`]), without the engine's echo of the call.
    pub lines: Vec<u64>,
    /// The call succeeded.
    pub ok: bool,
}

/// What a `tool_call_update` that ended a call returned. `None` for an update that is not the end of a call (`pending`, `in_progress`) and for an
/// end with no answer on the wire. The call failed when its status is `failed`, or when the answer says so in the ways the engine does:
/// `success: false`, `isError: true` or a non-empty `error`, at any of the first levels of the answer.
pub fn output_key(update: &Value) -> Option<Answer> {
    let status = update.get("status").and_then(Value::as_str)?;
    if status != "completed" && status != "failed" {
        return None;
    }
    let out = update.get("rawOutput").filter(|v| !v.is_null())?;
    let mut texts = Vec::new();
    strings_of(out, &mut texts);
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    'all: for text in texts {
        for line in text.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let mut h = DefaultHasher::new();
            "line".hash(&mut h);
            line.hash(&mut h);
            let h = h.finish();
            if seen.insert(h) {
                lines.push(h);
                if lines.len() >= MAX_LINES {
                    break 'all;
                }
            }
        }
    }
    Some(Answer {
        lines,
        ok: status == "completed" && !says_failed(out, 0),
    })
}

/// Every string of a JSON value, without the members named [`ECHO_KEY`] at any depth.
fn strings_of<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| strings_of(x, out)),
        Value::Object(m) => m
            .iter()
            .filter(|(k, _)| k.as_str() != ECHO_KEY)
            .for_each(|(_, x)| strings_of(x, out)),
        _ => {}
    }
}

fn says_failed(v: &Value, depth: u8) -> bool {
    match v {
        Value::Object(m) => {
            m.get("isError") == Some(&Value::Bool(true))
                || m.get("success") == Some(&Value::Bool(false))
                || m.get("error")
                    .is_some_and(|e| !e.is_null() && e.as_str() != Some(""))
                || (depth < 3 && m.values().any(|x| says_failed(x, depth + 1)))
        }
        Value::Array(a) => depth < 3 && a.iter().any(|x| says_failed(x, depth + 1)),
        _ => false,
    }
}

/// "2 hours", "20 minutes", or "90 seconds" when the span is no whole number of minutes.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = if s >= 3600 && s.is_multiple_of(3600) {
        (s / 3600, "hour")
    } else if s >= 60 && s.is_multiple_of(60) {
        (s / 60, "minute")
    } else {
        (s, "second")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}
