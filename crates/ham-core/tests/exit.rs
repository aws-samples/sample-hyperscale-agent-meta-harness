//! Exit codes map to run states.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::exit::{USAGE, exit_code};
use harness_protocol::goal::GoalStatus;

#[test]
fn each_status_has_a_distinct_exit_code() {
    let all = [
        GoalStatus::Complete,
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::UsageLimited,
        GoalStatus::BudgetLimited,
        GoalStatus::Cleared,
    ];
    let codes: Vec<i32> = all.iter().map(|s| exit_code(Some(*s))).collect();
    assert_eq!(exit_code(Some(GoalStatus::Complete)), 0);
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "{codes:?}");
    assert!(
        codes.iter().all(|c| *c != 1 && *c != USAGE),
        "1 and {USAGE} are reserved: {codes:?}"
    );
    assert_eq!(USAGE, 2);
    assert_ne!(exit_code(None), 0);
}
