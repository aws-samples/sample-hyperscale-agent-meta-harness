//! The goal-loop switch of a direct run, and its stall
//! limits and final-message switch. Protocol 1.46.0 adds the allowance of calls that bring something new.
//!
//! The engine's own goal plugin runs the goal loop by default. `ham ... --goal off` sends `goal: {"enabled": false}` and agentd then places
//! nothing for the plugin, writes no goal and starts no loop: the run is the plain run it was before the goal plugin (the nudge, which is off
//! unless the user's `continue.md` asks for it, behaves as it always did). Absent, or `enabled: true`, leaves the stage default, which is on.
//! `ham` sends the member only when the user says `--goal on` or `--goal off`, or sets one of the limits below, so a request that keeps the defaults
//! is byte for byte the request of an older client. The stage can still switch the loop off for every run (`AGENTD_GOAL`); a run cannot switch on
//! what the stage switched off.
//!
//! Five optional members (1.42.0) let the user move the numbers of the brake that ends a run which polls and changes nothing (the rule
//! `round-no-progress`), and switch off the one last turn Cline gets to write a final message before such a brake ends the run. A sixth, `newCalls`
//! (1.46.0), is the allowance of calls that bring something new and are not counted toward those numbers. Each is left off the
//! wire when the user keeps the default; the stage's `AGENTD_GOAL_*` settings are the defaults a member replaces. [`check_goal`] is the ONE check:
//! control-api (400 `invalid-goal`) and agentd (the run ends `stopped_setup_failed` before any model call) call it, so they accept the same
//! members. `ham` takes the bounds of its keys from the same constants, and a test holds every value it accepts to this check. The bounds are
//! the ones the stage settings are cut to, written once here.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// One whole-number limit of the goal loop: its default and the bounds a member is checked against (both ends included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoalRange {
    pub default: u32,
    pub min: u32,
    pub max: u32,
}

impl GoalRange {
    /// Whether `n` is inside the bounds.
    pub const fn contains(&self, n: u32) -> bool {
        n >= self.min && n <= self.max
    }
}

/// `roundCalls`: tool calls in a row, inside one round, with an unchanged tree and no new text, that end the run (stage `AGENTD_GOAL_ROUND_CALLS`).
pub const GOAL_ROUND_CALLS: GoalRange = GoalRange {
    default: 30,
    min: 5,
    max: 1000,
};
/// `roundWindowS`: seconds of an unchanged tree, with at least [`GOAL_ROUND_MIN_CALLS`] tool calls in them, that end the run (stage
/// `AGENTD_GOAL_ROUND_WINDOW_S`).
pub const GOAL_ROUND_WINDOW_S: GoalRange = GoalRange {
    default: 1200,
    min: 10,
    max: 14_400,
};
/// `roundMinCalls`: the least number of tool calls inside the window for the window to count (stage `AGENTD_GOAL_ROUND_MIN_CALLS`).
pub const GOAL_ROUND_MIN_CALLS: GoalRange = GoalRange {
    default: 10,
    min: 1,
    max: 100,
};
/// `commandGraceS`: seconds the tree may stay unchanged while a command of the engine's user is seen running, counted from the last
/// change; polling calls made while it runs are not counted against the two limits above (stage `AGENTD_GOAL_COMMAND_GRACE_S`). 0 does not look
/// for running commands at all: every call counts, as. The bounds are those of a value other than 0.
pub const GOAL_COMMAND_GRACE_S: GoalRange = GoalRange {
    default: 7200,
    min: 60,
    max: 86_400,
};
/// `newCalls` (protocol 1.46.0): between two changed files, this many tool calls whose input is new to the run, whose answer has at least three lines no earlier
/// answer had, and that succeeded, are not counted toward `roundCalls` and `roundMinCalls` (stage `AGENTD_GOAL_NEW_CALLS`). A model that reads and plans before it edits makes no file
/// change for as long as the task is large; the allowance lets it. A repeated call, an answer that repeats an earlier one or differs in a line or two, a failed call and every call beyond the allowance count. 0 counts
/// every call, as before 1.46.0. The bounds are those of a value other than 0.
pub const GOAL_NEW_CALLS: GoalRange = GoalRange {
    default: 100,
    min: 1,
    max: 1000,
};
/// `finalMessage`: whether Cline gets one last turn to write a final message before the brake inside a round ends the run (stage
/// `AGENTD_GOAL_FINAL`). On by default.
pub const GOAL_FINAL_MESSAGE: bool = true;

/// `StartDirectRunRequest.goal`: the user's goal-loop switch and limits. The first member has the shape of `research` (`{"enabled": bool}`), so a
/// client has one pattern for the switches of a run; the others (protocol 1.42.0) are optional numbers and a switch, each left out when unset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalOptions {
    pub enabled: bool,
    /// Protocol 1.42.0: tool calls in a row with an unchanged tree and no new text that end the run (5 to 1,000; the stage default is 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_calls: Option<u32>,
    /// Protocol 1.42.0: seconds of an unchanged tree that end the run when at least `roundMinCalls` tool calls came in them (10 to 14,400; the
    /// stage default is 1,200, 20 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_window_s: Option<u32>,
    /// Protocol 1.42.0: the least number of tool calls inside the window for the window to count (1 to 100; the stage default is 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_min_calls: Option<u32>,
    /// Protocol 1.42.0: seconds the tree may stay unchanged while a command is seen running before the run ends (60 to 86,400; the stage default is
    /// 7,200, two hours). While a command runs, the polling calls are not counted against the two limits above. 0 switches the look for running
    /// commands off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_grace_s: Option<u32>,
    /// Protocol 1.42.0: `false` ends such a run at once, as before; absent and `true` give Cline one last turn to write its final message first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<bool>,
    /// Protocol 1.46.0: the calls between two changed files that bring something new and are not counted (1 to 1,000; the stage default is 100).
    /// 0 counts every call, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_calls: Option<u32>,
    /// Protocol 1.48.0: `false` leaves the sentence about work outside the checkout out of the goal prompts (the goal text and the prompt
    /// that restarts a quiet loop); absent and `true` keep it. Stage default: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_checkout_note: Option<bool>,
}

impl GoalOptions {
    /// The switch that turns the goal loop off for one run.
    pub const OFF: GoalOptions = GoalOptions::switch(false);

    /// The plain switch (`--goal on` or `--goal off`): none of the limits is set.
    pub const fn switch(enabled: bool) -> GoalOptions {
        GoalOptions {
            enabled,
            round_calls: None,
            round_window_s: None,
            round_min_calls: None,
            command_grace_s: None,
            final_message: None,
            new_calls: None,
            outside_checkout_note: None,
        }
    }

    /// Whether the member says anything beyond the switch.
    pub fn has_limits(&self) -> bool {
        self.round_calls.is_some()
            || self.round_window_s.is_some()
            || self.round_min_calls.is_some()
            || self.command_grace_s.is_some()
            || self.final_message.is_some()
            || self.new_calls.is_some()
            || self.outside_checkout_note.is_some()
    }
}

/// Why a `goal` member was refused: the member that is out of its range. The first in the order of the members is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalProblem {
    RoundCalls,
    RoundWindow,
    RoundMinCalls,
    CommandGrace,
    NewCalls,
}

impl GoalProblem {
    /// The member's name on the wire.
    pub fn key(self) -> &'static str {
        match self {
            GoalProblem::RoundCalls => "roundCalls",
            GoalProblem::RoundWindow => "roundWindowS",
            GoalProblem::RoundMinCalls => "roundMinCalls",
            GoalProblem::CommandGrace => "commandGraceS",
            GoalProblem::NewCalls => "newCalls",
        }
    }

    /// A stable word for the kind of problem (one today).
    pub fn code(self) -> &'static str {
        "out_of_range"
    }

    /// The bounds of the member.
    pub fn range(self) -> GoalRange {
        match self {
            GoalProblem::RoundCalls => GOAL_ROUND_CALLS,
            GoalProblem::RoundWindow => GOAL_ROUND_WINDOW_S,
            GoalProblem::RoundMinCalls => GOAL_ROUND_MIN_CALLS,
            GoalProblem::CommandGrace => GOAL_COMMAND_GRACE_S,
            GoalProblem::NewCalls => GOAL_NEW_CALLS,
        }
    }

    /// The rule in words, without the value that broke it.
    pub fn rule(self) -> String {
        let r = self.range();
        match self {
            GoalProblem::CommandGrace => {
                format!(
                    "0 (do not look for running commands) or a whole number from {} to {}",
                    r.min, r.max
                )
            }
            GoalProblem::NewCalls => {
                format!(
                    "0 (count every call) or a whole number from {} to {}",
                    r.min, r.max
                )
            }
            _ => format!("a whole number from {} to {}", r.min, r.max),
        }
    }
}

impl fmt::Display for GoalProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "goal: {} must be {}", self.key(), self.rule())
    }
}

impl std::error::Error for GoalProblem {}

/// The ONE check of the `goal` member (protocol 1.42.0). Pure; the first problem in the order `roundCalls`, `roundWindowS`, `roundMinCalls`,
/// `commandGraceS`, `newCalls` is returned. The limits are checked also when `enabled` is false: a member that could not run if the loop were on is no member
/// to store. The message never repeats the value.
pub fn check_goal(g: &GoalOptions) -> Result<(), GoalProblem> {
    let rows = [
        (g.round_calls, GoalProblem::RoundCalls),
        (g.round_window_s, GoalProblem::RoundWindow),
        (g.round_min_calls, GoalProblem::RoundMinCalls),
        (g.command_grace_s, GoalProblem::CommandGrace),
        (g.new_calls, GoalProblem::NewCalls),
    ];
    for (value, problem) in rows {
        // 0 is the word of `commandGraceS` for "do not look for running commands" and of `newCalls` for "count every call".
        let zero_means_off = matches!(problem, GoalProblem::CommandGrace | GoalProblem::NewCalls);
        if let Some(n) = value
            && !problem.range().contains(n)
            && !(zero_means_off && n == 0)
        {
            return Err(problem);
        }
    }
    Ok(())
}
