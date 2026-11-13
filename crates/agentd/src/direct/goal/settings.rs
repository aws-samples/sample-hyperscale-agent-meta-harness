//! The settings of the goal module: the loop is on by default; every number here is the decision record's, a
//! value that does not parse is the default and one outside its bounds is cut to the bound. The limits of the brake inside a round, the grace
//! of a running command and the final-message switch have their defaults and bounds in `harness_protocol::direct` (`GOAL_*`), the one place the
//! client and the control plane also read; a start request's `goal` member replaces them for one run ([`GoalSettings::with_request`]).

use crate::direct::goal::machine::Limits;
use crate::direct::goal::watch::{HelperLimits, WatchLimits};
use harness_protocol::direct::{
    GOAL_COMMAND_GRACE_S, GOAL_NEW_CALLS, GOAL_ROUND_CALLS, GOAL_ROUND_MIN_CALLS,
    GOAL_ROUND_WINDOW_S, GoalOptions, GoalRange,
};
use std::path::PathBuf;
use std::time::Duration;

/// The sentence about work outside the checkout that ends the default goal text and the prompt that restarts a quiet loop. A user can leave it
/// out: [`strip_outside_note`] takes it, and the space before it, out of a text.
pub const OUTSIDE_CHECKOUT_NOTE: &str =
    "Work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest.";

/// `text` without [`OUTSIDE_CHECKOUT_NOTE`] and the space in front of it; a text that does not hold the sentence is returned as it is.
pub fn strip_outside_note(text: &str) -> String {
    text.replace(&format!(" {OUTSIDE_CHECKOUT_NOTE}"), "")
        .replace(OUTSIDE_CHECKOUT_NOTE, "")
        .trim_end()
        .to_string()
}

/// The goal the engine is given when nobody wrote another. It is short on purpose: the plugin quotes it in every follow-up prompt.
pub const DEFAULT_GOAL_TEXT: &str = "Finish the task you were given. If the lifecycle goal card exists (.ham/lifecycle/GOAL.md, or the same path with the folder that \
.ham/records names in place of .ham), its numbered checks are the goal: every check must pass in a run made after your last edit. Answer each question about \
the goal from the output of those checks, not from memory. If you cannot make progress, end your final message with a line that starts with \
BLOCKED: and says what is missing. Work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest.";

/// [`DEFAULT_GOAL_TEXT`] names the records folder `.ham`. In a run that works in another folder (COMPAT-CAH-DATA: `.cah`) the default text names that
/// folder in its place, so the plugin's follow-up prompts agree with the first line of the first prompt. A text the user wrote is returned as it is.
pub fn goal_text_in(folder: &str, text: &str) -> String {
    let head = DEFAULT_GOAL_TEXT
        .split_once(" its numbered checks")
        .map_or(DEFAULT_GOAL_TEXT, |(head, _)| head);
    match text.strip_prefix(head) {
        Some(rest) if folder != crate::direct::records::DEFAULT_BASE => format!(
            "{}{rest}",
            head.replace(crate::direct::records::DEFAULT_BASE, folder)
        ),
        _ => text.to_string(),
    }
}

/// The least window, when the stage names none: a test run that waits for timeouts can be quiet for ten minutes at a time after minutes of work,
/// and the stall window (20 minutes) alone would release a suite of 25 minutes whose work came at the start.
pub const COMMAND_WINDOW_FLOOR: Duration = Duration::from_secs(30 * 60);

/// The longest goal text, in bytes.
pub const MAX_GOAL_TEXT: usize = 2000;
/// Where the worker image keeps the plugin folders.
pub const DEFAULT_PLUGIN_DIR: &str = "/opt/harness/engine/plugins";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalSettings {
    /// `AGENTD_GOAL`: `off`, `0`, `false`, `no` or `disabled` switches the goal loop off; anything else leaves it on.
    pub enabled: bool,
    /// `AGENTD_GOAL_PLUGIN_DIR`: the folder that holds `lock.json` and the plugin folders.
    pub plugin_dir: PathBuf,
    /// `AGENTD_GOAL_TEXT`: the goal text (blank is the default).
    pub goal_text: String,
    blocked_rounds: u32,
    stall_rounds: u32,
    idle_rounds: u32,
    kicks_max: u32,
    kick_after: Duration,
    dead_grace: Duration,
    settle: Duration,
    fallback_quiet: Duration,
    cleared_confirm: Duration,
    /// `AGENTD_GOAL_CLAIM_WAIT_MS`: how long the module waits at the first turn end for the engine to claim the goal before it says the
    /// engine did not (the state file is rewritten by the plugin without a lock, so one read can meet half a write).
    pub claim_wait: Duration,
    /// `AGENTD_GOAL_DRAIN_S`: how long a round in flight may finish after the loop was told to stop.
    pub drain: Duration,
    /// `AGENTD_GOAL_POLL_MS`: how often the supervisor looks at the rounds log.
    pub poll: Duration,
    /// `AGENTD_GOAL_EVENTS_EVERY_S`: the least time between two round events in the run's event log.
    pub events_every: Duration,
    /// `AGENTD_GOAL_ROUND_CALLS` (30): tool calls in a row, inside one round, with an unchanged tree and no new text, that end the run.
    pub round_calls: u32,
    /// `AGENTD_GOAL_ROUND_WINDOW_S` (1200 s, 20 minutes): wall-clock time inside one round with an unchanged tree that ends the run.
    pub round_window: Duration,
    /// `AGENTD_GOAL_ROUND_MIN_CALLS` (10): the least number of tool calls inside the window for the window to count.
    pub round_min_calls: u32,
    /// `AGENTD_GOAL_LOOK_EVERY_MS` (5000): the least time between two looks at the tree while tool calls come, and between two samples of the
    /// evidence that a command is running.
    pub look_every: Duration,
    /// `AGENTD_GOAL_COMMAND_GRACE_S` (7200, 60 to 86400, or 0): how long the tree may stay unchanged while a command of the engine's user is seen
    /// running before the run ends (never less than the round window). 0 stops the module from looking at the engine user's processes.
    pub command_grace: Duration,
    /// `AGENTD_GOAL_COMMAND` (on): `off` and the other off words stop the module from looking at the engine user's processes; the brake then
    /// counts every call, as before. A grace of 0 does the same ([`GoalSettings::looks_for_commands`]).
    pub command_evidence: bool,
    /// `AGENTD_GOAL_COMMAND_CPU_PCT` (5, 1 to 100): the share of one CPU, in percent, that processes outside the session's baseline must use over
    /// the time between two samples for a command to count as running.
    pub command_cpu_pct: u32,
    /// `AGENTD_GOAL_COMMAND_WINDOW_S` (5 to 14400, or 0): how far back the evidence that a command is running reaches: the sessions that are
    /// still there count as a command when they used the rate (`command_cpu_pct`) over this time, not only in the last sample interval. 0 judges
    /// every interval alone. Not set: the longer of the stall window of the run and [`COMMAND_WINDOW_FLOOR`] ([`GoalSettings::command_window`]).
    /// Read through `command_window()`.
    command_window: Option<Duration>,
    /// `AGENTD_STALL_BUSY_PCT` (50, 0 to 1000; 0 switches the signal off): the share of one CPU, in percent, that the session outside agentd and the
    /// engine must have used over a stretch for the brake to count the stretch as progress.
    pub busy_pct: u32,
    /// `AGENTD_GOAL_NEW_CALLS` (100, 1 to 1000, or 0 or `off`): between two changed files, this many tool calls whose input is new to the
    /// run, whose answer has at least three lines no earlier answer had, and that succeeded, are not counted toward `round_calls` and
    /// `round_min_calls`. 0 counts every call.
    pub new_calls: u32,
    /// `AGENTD_GOAL_HELPER_SILENCE_S` (1500, 1 to 86400, or 0): how long the engine may wait for its helper agents with no helper call answered
    /// before the module cancels the prompt and tells the engine. 0 switches the rule off. Gate runs whose helpers all returned had at most
    /// 19.4 minutes between two answers; the two runs that were stalled had 29 and 49.
    pub helper_silence: Duration,
    /// `AGENTD_GOAL_HELPER_WAIT_S` (4500, 1 to 86400, or 0): how old the oldest open helper call may get before the same cancel. 0 switches
    /// the rule off. The longest wait of a gate run whose helpers all returned was 55.8 minutes.
    pub helper_wait: Duration,
    /// `AGENTD_GOAL_FINAL` (on): `off` and the other off words end a run at once when the brake inside a round fires, without the last turn in
    /// which the engine writes its final message.
    pub final_message: bool,
    /// `AGENTD_GOAL_FINAL_S` (120, 5 to 600): the longest that last turn may take; it is cut short to fit what is left of the run's deadline.
    pub final_window: Duration,
    /// Whether the goal prompts carry [`OUTSIDE_CHECKOUT_NOTE`].
    pub outside_note: bool,
}

fn number(
    get: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> u64 {
    get(key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(default, |v| v.clamp(min, max))
}

fn count(get: &dyn Fn(&str) -> Option<String>, key: &str, default: u32, min: u32, max: u32) -> u32 {
    u32::try_from(number(
        get,
        key,
        u64::from(default),
        u64::from(min),
        u64::from(max),
    ))
    .unwrap_or(default)
}

/// `AGENTD_GOAL_COMMAND_GRACE_S`: 0 (do not look for running commands) or seconds cut to the bounds of the protocol range.
fn grace(get: &dyn Fn(&str) -> Option<String>) -> Duration {
    let r = GOAL_COMMAND_GRACE_S;
    let n = get("AGENTD_GOAL_COMMAND_GRACE_S")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(u64::from(r.default), |n| {
            if n == 0 {
                0
            } else {
                n.clamp(u64::from(r.min), u64::from(r.max))
            }
        });
    Duration::from_secs(n)
}

/// A limit of the wait for helper agents: 0 switches the rule off, anything else is cut to 1 to 86,400 seconds; a value that does not
/// parse is the default.
fn helper_limit(get: &dyn Fn(&str) -> Option<String>, key: &str, default: u64) -> Duration {
    let n = get(key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(default, |n| if n == 0 { 0 } else { n.clamp(1, 86_400) });
    Duration::from_secs(n)
}

/// `AGENTD_GOAL_COMMAND_WINDOW_S`: 0 (no window) or seconds cut to 5 to 14,400; not set, or a value that does not parse, leaves the default: the
/// longer of the run's stall window and 30 minutes ([`GoalSettings::command_window`]).
fn command_window(get: &dyn Fn(&str) -> Option<String>) -> Option<Duration> {
    let n = get("AGENTD_GOAL_COMMAND_WINDOW_S")?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(if n == 0 {
        0
    } else {
        n.clamp(5, 14_400)
    }))
}

/// `AGENTD_GOAL_NEW_CALLS`: 0 (count every call; `off` and the other off words say the same) or a number cut to the bounds of the protocol range.
fn new_calls(get: &dyn Fn(&str) -> Option<String>) -> u32 {
    let r = GOAL_NEW_CALLS;
    let Some(v) = get("AGENTD_GOAL_NEW_CALLS") else {
        return r.default;
    };
    let v = v.trim().to_ascii_lowercase();
    if matches!(v.as_str(), "off" | "false" | "no" | "disabled") {
        return 0;
    }
    v.parse::<u32>().map_or(
        r.default,
        |n| if n == 0 { 0 } else { n.clamp(r.min, r.max) },
    )
}

/// A whole number with the default and the bounds of a protocol range.
fn ranged(get: &dyn Fn(&str) -> Option<String>, key: &str, r: GoalRange) -> u32 {
    count(get, key, r.default, r.min, r.max)
}

impl GoalSettings {
    /// The settings a lookup of environment variables gives (`std::env::var` in a run).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let on = |key: &str| {
            !get(key).is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "no" | "disabled"
                )
            })
        };
        let enabled = on("AGENTD_GOAL");
        let goal_text = get("AGENTD_GOAL_TEXT")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .map_or_else(
                || DEFAULT_GOAL_TEXT.to_string(),
                |t| cut_bytes(&t, MAX_GOAL_TEXT).to_string(),
            );
        let secs =
            |key, default, min, max| Duration::from_secs(number(get, key, default, min, max));
        let millis =
            |key, default, min, max| Duration::from_millis(number(get, key, default, min, max));
        Self {
            enabled,
            plugin_dir: get("AGENTD_GOAL_PLUGIN_DIR")
                .filter(|p| !p.trim().is_empty())
                .map_or_else(|| PathBuf::from(DEFAULT_PLUGIN_DIR), PathBuf::from),
            goal_text,
            blocked_rounds: count(get, "AGENTD_GOAL_BLOCKED_ROUNDS", 2, 1, 10),
            stall_rounds: count(get, "AGENTD_GOAL_STALL_ROUNDS", 3, 2, 20),
            idle_rounds: count(get, "AGENTD_GOAL_IDLE_ROUNDS", 5, 3, 50),
            kicks_max: count(get, "AGENTD_GOAL_KICKS_MAX", 2, 0, 5),
            kick_after: secs("AGENTD_GOAL_KICK_AFTER_S", 15, 1, 600),
            dead_grace: millis("AGENTD_GOAL_DEAD_GRACE_MS", 2000, 0, 60_000),
            settle: millis("AGENTD_GOAL_SETTLE_MS", 3000, 0, 60_000),
            fallback_quiet: secs("AGENTD_GOAL_FALLBACK_QUIET_S", 15, 1, 600),
            cleared_confirm: millis("AGENTD_GOAL_CLEARED_CONFIRM_MS", 2000, 0, 60_000),
            claim_wait: millis("AGENTD_GOAL_CLAIM_WAIT_MS", 3000, 0, 60_000),
            drain: secs("AGENTD_GOAL_DRAIN_S", 120, 1, 600),
            poll: millis("AGENTD_GOAL_POLL_MS", 1000, 10, 10_000),
            events_every: secs("AGENTD_GOAL_EVENTS_EVERY_S", 5, 0, 600),
            round_calls: ranged(get, "AGENTD_GOAL_ROUND_CALLS", GOAL_ROUND_CALLS),
            round_window: secs(
                "AGENTD_GOAL_ROUND_WINDOW_S",
                u64::from(GOAL_ROUND_WINDOW_S.default),
                u64::from(GOAL_ROUND_WINDOW_S.min),
                u64::from(GOAL_ROUND_WINDOW_S.max),
            ),
            round_min_calls: ranged(get, "AGENTD_GOAL_ROUND_MIN_CALLS", GOAL_ROUND_MIN_CALLS),
            look_every: millis("AGENTD_GOAL_LOOK_EVERY_MS", 5000, 0, 60_000),
            command_grace: grace(get),
            command_evidence: on("AGENTD_GOAL_COMMAND"),
            command_cpu_pct: count(get, "AGENTD_GOAL_COMMAND_CPU_PCT", 5, 1, 100),
            command_window: command_window(get),
            busy_pct: count(get, "AGENTD_STALL_BUSY_PCT", 50, 0, 1000),
            new_calls: new_calls(get),
            helper_silence: helper_limit(get, "AGENTD_GOAL_HELPER_SILENCE_S", 1500),
            helper_wait: helper_limit(get, "AGENTD_GOAL_HELPER_WAIT_S", 4500),
            final_message: on("AGENTD_GOAL_FINAL"),
            final_window: secs("AGENTD_GOAL_FINAL_S", 120, 5, 600),
            outside_note: true,
        }
    }

    /// These settings with the members of a start request's `goal` object laid over them: a member the user set replaces the stage's
    /// value for this run, a member left out keeps it. The request was checked by `harness_protocol::direct::check_goal`; each value is cut to
    /// its bounds again here, so a number that was not checked cannot get past them.
    pub fn with_request(&self, g: &GoalOptions) -> Self {
        let mut s = self.clone();
        let cut = |n: u32, r: GoalRange| n.clamp(r.min, r.max);
        if let Some(n) = g.round_calls {
            s.round_calls = cut(n, GOAL_ROUND_CALLS);
        }
        if let Some(n) = g.round_window_s {
            s.round_window = Duration::from_secs(u64::from(cut(n, GOAL_ROUND_WINDOW_S)));
        }
        if let Some(n) = g.round_min_calls {
            s.round_min_calls = cut(n, GOAL_ROUND_MIN_CALLS);
        }
        if let Some(n) = g.command_grace_s {
            // 0 is the request's word for "do not look for running commands"
            s.command_grace = Duration::from_secs(if n == 0 {
                0
            } else {
                u64::from(cut(n, GOAL_COMMAND_GRACE_S))
            });
        }
        if let Some(on) = g.final_message {
            s.final_message = on;
        }
        if let Some(n) = g.new_calls {
            // 0 is the request's word for "count every call"
            s.new_calls = if n == 0 { 0 } else { cut(n, GOAL_NEW_CALLS) };
        }
        if g.outside_checkout_note == Some(false) && s.outside_note {
            s.outside_note = false;
            s.goal_text = strip_outside_note(&s.goal_text);
        }
        s
    }

    /// The prompt that restarts a quiet loop, with or without the sentence about work outside the checkout.
    pub fn kick_prompt(&self) -> String {
        crate::direct::goal::kick_prompt(self.outside_note)
    }

    /// How far back the evidence that a command is running reaches: the stage's `AGENTD_GOAL_COMMAND_WINDOW_S` when it names one, else the longer
    /// of the stall window of the run (a request's `roundWindowS` moves it) and [`COMMAND_WINDOW_FLOOR`]. Zero means every interval alone.
    pub fn command_window(&self) -> Duration {
        self.command_window
            .unwrap_or_else(|| self.round_window.max(COMMAND_WINDOW_FLOOR))
    }

    /// Whether the module looks at the engine user's processes for a running command: the stage switch is on and the grace is not 0.
    pub fn looks_for_commands(&self) -> bool {
        self.command_evidence && !self.command_grace.is_zero()
    }

    /// Whether the brake reads the session's CPU as a second signal of progress.
    pub fn busy_signal(&self) -> bool {
        self.busy_pct > 0 && self.looks_for_commands()
    }

    /// The settings of the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }

    /// The limits of the wait for helper agents.
    pub fn helper_limits(&self) -> HelperLimits {
        HelperLimits {
            silence: self.helper_silence,
            wait: self.helper_wait,
        }
    }

    /// The limits of the brake inside a round, for the engine called `who` in the words of a detail.
    pub fn watch_limits(&self, who: &'static str) -> WatchLimits {
        WatchLimits {
            calls: self.round_calls,
            window: self.round_window,
            min_calls: self.round_min_calls,
            who,
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            who: "the engine",
            blocked_rounds: self.blocked_rounds,
            stall_rounds: self.stall_rounds,
            idle_rounds: self.idle_rounds,
            kick_after: self.kick_after,
            dead_grace: self.dead_grace,
            kicks_max: self.kicks_max,
            settle: self.settle,
            fallback_quiet: self.fallback_quiet,
            cleared_confirm: self.cleared_confirm,
        }
    }
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn cut_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
