//! Process exit codes.
//!
//! 0 is `complete`; 1 is an error before a run state is known; 2 is a usage error; 11 is `paused` and 16 is `stalled`;
//! each other goal
//! status has its own code so scripts can branch on the result of `ham watch` or `ham "<prompt>"`.

use harness_protocol::goal::GoalStatus;

pub const ERROR: i32 = 1;
pub const USAGE: i32 = 2;

pub fn exit_code(status: Option<GoalStatus>) -> i32 {
    match status {
        Some(GoalStatus::Complete) => 0,
        Some(GoalStatus::Active) => 10,
        Some(GoalStatus::Paused) => 11,
        Some(GoalStatus::Blocked) => 12,
        Some(GoalStatus::UsageLimited) => 13,
        Some(GoalStatus::BudgetLimited) => 14,
        Some(GoalStatus::Cleared) => 15,
        Some(GoalStatus::Stalled) => 16,
        None => ERROR,
    }
}
