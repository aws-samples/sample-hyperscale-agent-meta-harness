//! Wave 2, the infra lane: the longest total time of a run, the most sessions it may use, and what must move with them.
//!
//! The run machine is an AWS Step Functions standard workflow. One execution keeps at most 25,000 history events and runs for at most a year, so the
//! ceiling a user may ask for is chosen from those two numbers. The numbers below that come from the files of this repository (the machine's
//! `TimeoutSeconds`, its retry counts, the CDK app's constant, the snapshot grant of the control stack) are read from the files, so a later change
//! to one of them cannot leave the others behind.

use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, MAX_DIRECT_TIMEOUT_S, MAX_MAX_RUN_S, MAX_SLICES, MIN_MAX_RUN_S, max_run_s_ok,
};
use serde_json::Value;
use std::path::PathBuf;

const DAY_S: u32 = 24 * 3600;
/// Step Functions keeps at most this many history events of one execution (the quota of the service).
const SFN_HISTORY_EVENTS: u64 = 25_000;
/// A standard execution runs for one year at most.
const SFN_MAX_EXECUTION_S: u64 = 365 * 24 * 3600;
/// The seconds the machine adds to the ceiling for staging the repository and for the delivery (ten hours).
const MACHINE_EXTRA_S: u64 = 36_000;

fn repo_file(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_file(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn machine() -> Value {
    serde_json::from_str(&read("statemachines/direct.asl.json")).expect("direct.asl.json is JSON")
}

/// The most history events one visit of a state can add, retries included: a task writes entered, scheduled, started, succeeded and exited (a
/// task that waits for a token also writes submitted) and each retry writes a failure, a new schedule and a new start; a choice and a pass write two.
fn worst_events(state: &Value) -> u64 {
    match state["Type"].as_str().expect("a state has a Type") {
        "Task" => {
            let token = state["Resource"]
                .as_str()
                .is_some_and(|r| r.ends_with(".waitForTaskToken"));
            let retries: u64 = state["Retry"].as_array().map_or(0, |rs| {
                rs.iter()
                    .map(|r| r["MaxAttempts"].as_u64().unwrap_or(3))
                    .sum()
            });
            5 + u64::from(token) + 3 * retries
        }
        "Choice" | "Pass" => 2,
        "Fail" | "Succeed" => 1,
        other => panic!("a state type this test does not count: {other}"),
    }
}

#[test]
fn b776w_a_run_may_take_thirty_days_in_all() {
    assert_eq!(MAX_MAX_RUN_S, 30 * DAY_S);
    assert!(max_run_s_ok(MAX_MAX_RUN_S));
    assert!(!max_run_s_ok(MAX_MAX_RUN_S + 1));
    // The default and the least stay what they were: a user who sets nothing sees no change.
    assert_eq!(DEFAULT_MAX_RUN_S, DAY_S);
    assert!(
        max_run_s_ok(DEFAULT_MAX_RUN_S)
            && max_run_s_ok(MIN_MAX_RUN_S)
            && !max_run_s_ok(MIN_MAX_RUN_S - 1)
    );
}

#[test]
fn b776w_the_machine_outlasts_the_longest_run_and_stays_under_a_year() {
    let asl = machine();
    let timeout = asl["TimeoutSeconds"]
        .as_u64()
        .expect("the machine has a TimeoutSeconds");
    assert_eq!(timeout, u64::from(MAX_MAX_RUN_S) + MACHINE_EXTRA_S);
    assert!(
        timeout < SFN_MAX_EXECUTION_S,
        "{timeout} s is not under the one year Step Functions allows"
    );
}

#[test]
fn b776w_the_cdk_app_names_the_same_machine_timeout_as_the_machine() {
    let ts = read("infra/lib/stacks/orchestration-direct.ts");
    let line = ts
        .lines()
        .find(|l| l.starts_with("export const RUN_TIMEOUT_S"))
        .expect("orchestration-direct.ts exports RUN_TIMEOUT_S");
    let digits: String = line
        .split('=')
        .nth(1)
        .expect("an assignment")
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let in_app: u64 = digits
        .parse()
        .expect("RUN_TIMEOUT_S is a number written out");
    assert_eq!(in_app, machine()["TimeoutSeconds"].as_u64().unwrap());
}

#[test]
fn b776w_the_session_count_covers_the_ceiling_twice_over() {
    // Every session but the last runs to its prompt deadline when the work is long; a lost or empty session is retried (three in a row at most),
    // so a run may use twice the sessions its time needs.
    let needed = u64::from(MAX_MAX_RUN_S).div_ceil(u64::from(MAX_DIRECT_TIMEOUT_S));
    assert!(
        u64::from(MAX_SLICES) >= 2 * needed,
        "{MAX_SLICES} sessions for {needed} full ones"
    );
}

#[test]
fn b776w_the_history_of_the_longest_run_fits_the_machine_limit_with_room() {
    let asl = machine();
    let states = asl["States"].as_object().expect("States");
    let in_loop = ["StartSession", "NextSlice", "SliceChoice", "ResetSession"];
    let per_session: u64 = in_loop.iter().map(|n| worst_events(&states[*n])).sum();
    let once: u64 = states
        .iter()
        .filter(|(n, _)| !in_loop.contains(&n.as_str()))
        .map(|(_, s)| worst_events(s))
        .sum();
    let worst = u64::from(MAX_SLICES) * per_session + once;
    // At most four fifths of the quota: the rest is for events the count does not model (a catch that reaches a state twice).
    assert!(
        worst * 5 <= SFN_HISTORY_EVENTS * 4,
        "{MAX_SLICES} sessions of {per_session} events and {once} once is {worst} events"
    );
}

#[test]
fn b776w_the_snapshot_download_grant_names_every_turn_number_up_to_the_session_count() {
    // IAM `?` is one character, and the grant names the turn directory without a wildcard that spans a path, so one pattern is needed for each
    // digit count of a turn number: 1 to 9, 10 to 99 and, from 100 sessions, 100 to 999.
    let ts = read("infra/lib/stacks/control.ts");
    let mut widths: Vec<usize> = Vec::new();
    for part in ts.split("agents/direct/turns/").skip(1) {
        let marks = part.chars().take_while(|c| *c == '?').count();
        if marks > 0 && part[marks..].starts_with("/snapshot.bundle") {
            widths.push(marks);
        }
    }
    widths.sort_unstable();
    widths.dedup();
    let digits = MAX_SLICES.to_string().len();
    assert_eq!(
        widths,
        (1..=digits).collect::<Vec<_>>(),
        "turn directory patterns of the snapshot grant"
    );
}

#[test]
fn b776w_the_machine_comment_names_the_session_count() {
    let asl = machine();
    let comment = asl["Comment"].as_str().expect("Comment");
    assert!(
        comment.contains(&format!("at most {MAX_SLICES} slices")),
        "{comment}"
    );
}
