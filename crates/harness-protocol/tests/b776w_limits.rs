//! Wave 2 (client and backend lambdas): the limits whose raise needs a pinned reason or a number the protocol owns.
//!
//! * The longest wait for an answer is the longest run, because the agent cuts the window to the time its session has left.
//! * The run ceiling and the slice count stay inside what the state machine and the Step Functions event history hold. The last test is a
//!   pin: it reads `statemachines/direct.asl.json` and fails when somebody raises one number and not the others.
//! * A search may ask for the results and the domains that the search providers take.
//! * A policy row carries as many prompt URLs as one DynamoDB item holds.
//!
//! Wave 2, rows C9, C30 and C35: the `limits` member of a start request. Six optional numbers move the interrupt count, the questions and
//! fresh sessions a run may have, and the research material budgets; the one check is shared by control-api and agentd.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DEFAULT_MAX_RUN_S, LIMIT_FRESH_SESSIONS, LIMIT_INTERRUPTS, LIMIT_QUESTIONS,
    LIMIT_RESEARCH_FILE_BYTES, LIMIT_RESEARCH_RUN_BYTES, LIMIT_RESEARCH_UNPACKED_BYTES, LimitRange,
    LimitsProblem, MAX_DIRECT_TIMEOUT_S, MAX_INPUT_REQUESTS, MAX_INPUT_WAIT_S, MAX_MAX_RUN_S,
    MAX_SLICES, MAX_USER_SESSIONS, MIN_MAX_RUN_S, RunLimits, check_limits, max_run_s_ok,
};
use harness_protocol::research::{
    MAX_INCLUDE_DOMAINS, MAX_PROMPT_URLS, MAX_SEARCH_RESULTS, PolicyItem, ResearchLimits,
    ResearchProvider, SearchRequest,
};
use serde_json::{Value, json};

const RUN: &str = "01M6PAKMAKGS9HBQ0S7PZTJX4T";
const MIB: u64 = 1024 * 1024;

#[test]
fn b776w_the_longest_input_wait_is_the_longest_run() {
    // The agent cuts every window to the time its session has left less 900 s, so the value a request may carry needs no bound of its own.
    assert_eq!(MAX_INPUT_WAIT_S, MAX_MAX_RUN_S);
    let wait = std::hint::black_box(MAX_INPUT_WAIT_S);
    assert!(wait > 4 * 3600, "the old 4 hour ceiling is gone");
    assert!(max_run_s_ok(DEFAULT_MAX_RUN_S) && max_run_s_ok(MIN_MAX_RUN_S));
}

fn state_machine() -> Value {
    let path = format!(
        "{}/../../statemachines/direct.asl.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

/// The history events one pass of a state costs at most, with every retry used: a Lambda task enters, schedules, starts and succeeds and exits
/// (`.waitForTaskToken` adds the submit), and each retry adds a failure, a schedule and a start. A choice and a pass state cost two.
fn worst_events(state: &Value) -> u64 {
    match state["Type"].as_str().unwrap() {
        "Task" => {
            let token = state["Resource"]
                .as_str()
                .unwrap()
                .ends_with(".waitForTaskToken");
            let retries: u64 = state["Retry"]
                .as_array()
                .map(|r| {
                    r.iter()
                        .map(|b| b["MaxAttempts"].as_u64().unwrap_or(3))
                        .sum()
                })
                .unwrap_or(0);
            5 + u64::from(token) + 3 * retries
        }
        "Choice" | "Pass" => 2,
        other => panic!("a {other} state in the slice loop"),
    }
}

#[test]
fn b776w_the_run_ceiling_fits_the_state_machine_and_its_event_history() {
    let def = state_machine();
    // The machine lives as long as the longest run plus ten hours for the stage preparation and the delivery, and never past one year.
    let timeout = def["TimeoutSeconds"].as_u64().unwrap();
    assert_eq!(
        timeout,
        u64::from(MAX_MAX_RUN_S) + 36_000,
        "raise the state machine timeout, its infra test and MAX_MAX_RUN_S together"
    );
    assert!(
        timeout <= 365 * 24 * 3600,
        "a Step Functions standard execution runs a year at most"
    );
    // A run of the longest total needs this many full sessions; the slice count leaves room for the same number again for restarts.
    let full = u64::from(MAX_MAX_RUN_S).div_ceil(u64::from(MAX_DIRECT_TIMEOUT_S));
    assert!(
        u64::from(MAX_SLICES) >= 2 * full,
        "{MAX_SLICES} slices cannot hold {full} full sessions and their restarts"
    );
    // The loop is StartSession, NextSlice, SliceChoice and ResetSession; the history keeps 25,000 events, and the preparation and the end take 200.
    let states = &def["States"];
    let per_slice: u64 = ["StartSession", "NextSlice", "SliceChoice", "ResetSession"]
        .iter()
        .map(|n| worst_events(&states[*n]))
        .sum();
    let history = u64::from(MAX_SLICES) * per_slice + 200;
    assert!(
        history <= 25_000,
        "{MAX_SLICES} slices of up to {per_slice} events fill {history} of the 25,000 events"
    );
}

#[test]
fn b776w_a_search_may_ask_for_20_results_and_300_domains() {
    assert_eq!(MAX_SEARCH_RESULTS, 20);
    assert_eq!(MAX_INCLUDE_DOMAINS, 300);
    let domains: Vec<String> = (0..300).map(|i| format!("docs{i}.example.org")).collect();
    let ok = SearchRequest {
        query: "tokio runtime shutdown".into(),
        max_results: Some(20),
        include_domains: domains.clone(),
        ..SearchRequest::default()
    };
    ok.validate(&ResearchLimits::default()).unwrap();
    let too_many = SearchRequest {
        max_results: Some(21),
        ..ok.clone()
    };
    let e = too_many
        .validate(&ResearchLimits::default())
        .unwrap_err()
        .to_string();
    assert!(e.contains("1 to 20"), "{e}");
    let mut more = domains;
    more.push("one-more.example.org".into());
    let e = SearchRequest {
        include_domains: more,
        ..ok
    }
    .validate(&ResearchLimits::default())
    .unwrap_err()
    .to_string();
    assert!(e.contains("300"), "{e}");
}

#[test]
fn b776w_a_policy_row_holds_150_prompt_urls_of_full_length_inside_one_dynamodb_item() {
    assert_eq!(MAX_PROMPT_URLS, 150);
    let url_bytes = ResearchLimits::default().url_bytes as usize;
    let url = |i: usize| {
        let head = format!("https://example.org/{i:04}/");
        format!("{head}{}", "a".repeat(url_bytes - head.len()))
    };
    let urls: Vec<String> = (0..MAX_PROMPT_URLS).map(url).collect();
    assert!(urls.iter().all(|u| u.len() == url_bytes));
    let row = PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Tavily,
        ResearchLimits::default(),
        1_900_000_000,
    )
    .with_prompt_urls(urls.clone());
    row.validate().unwrap();
    let size = serde_json::to_vec(&row).unwrap().len();
    assert!(
        size < 400 * 1024,
        "{size} bytes do not fit one DynamoDB item (400 KiB)"
    );
    let mut over = urls;
    over.push(url(MAX_PROMPT_URLS));
    let row = PolicyItem::new(
        RUN,
        true,
        ResearchProvider::Tavily,
        ResearchLimits::default(),
        1_900_000_000,
    )
    .with_prompt_urls(over);
    assert!(row.validate().is_err(), "151 URLs are refused");
}

fn parse(limits: Option<Value>) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut body = json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": []
    });
    if let Some(l) = limits {
        body["limits"] = l;
    }
    serde_json::from_value(body)
}

#[test]
fn b776w_c9_the_defaults_are_the_numbers_of_today() {
    assert_eq!(LIMIT_INTERRUPTS.default, 5);
    assert_eq!(LIMIT_QUESTIONS.default, u64::from(MAX_INPUT_REQUESTS));
    assert_eq!(LIMIT_FRESH_SESSIONS.default, u64::from(MAX_USER_SESSIONS));
    assert_eq!(LIMIT_RESEARCH_FILE_BYTES.default, MIB);
    assert_eq!(LIMIT_RESEARCH_UNPACKED_BYTES.default, 200 * MIB);
    assert_eq!(LIMIT_RESEARCH_RUN_BYTES.default, 300 * MIB);
    for r in [
        LIMIT_INTERRUPTS,
        LIMIT_QUESTIONS,
        LIMIT_FRESH_SESSIONS,
        LIMIT_RESEARCH_FILE_BYTES,
        LIMIT_RESEARCH_UNPACKED_BYTES,
        LIMIT_RESEARCH_RUN_BYTES,
    ] {
        assert!(
            r.contains(r.default),
            "{r:?}: the default is inside its range"
        );
    }
}

#[test]
fn b776w_c9_a_request_without_limits_serializes_without_the_member() {
    let r = parse(None).unwrap();
    assert_eq!(r.limits, None);
    let v = serde_json::to_value(&r).unwrap();
    assert!(v.get("limits").is_none(), "{v}");
    // An empty member is nothing to send: the client leaves it off.
    assert!(RunLimits::default().is_empty());
    assert!(serde_json::to_value(RunLimits::default()).unwrap() == json!({}));
}

#[test]
fn b776w_c9_the_member_round_trips_with_every_field() {
    let sent = json!({
        "interrupts": 20, "questions": 10, "freshSessions": 7,
        "researchFileBytes": 4 * MIB, "researchUnpackedBytes": 500 * MIB, "researchRunBytes": 900 * MIB
    });
    let r = parse(Some(sent.clone())).unwrap();
    let l = r.limits.unwrap();
    assert_eq!(l.interrupts, Some(20));
    assert_eq!(l.questions, Some(10));
    assert_eq!(l.fresh_sessions, Some(7));
    assert_eq!(l.research_file_bytes, Some(4 * MIB));
    assert_eq!(l.research_unpacked_bytes, Some(500 * MIB));
    assert_eq!(l.research_run_bytes, Some(900 * MIB));
    assert_eq!(check_limits(&l), Ok(()));
    assert_eq!(serde_json::to_value(l).unwrap(), sent);
    assert!(!l.is_empty());
}

#[test]
fn b776w_c9_an_unknown_member_of_limits_is_refused_by_the_wire_type() {
    assert!(parse(Some(json!({"interrupts": 3, "maxTurns": 9}))).is_err());
    assert!(parse(Some(json!({"interrupts": -1}))).is_err());
    assert!(parse(Some(json!({"interrupts": "five"}))).is_err());
}

fn with(f: impl FnOnce(&mut RunLimits)) -> RunLimits {
    let mut l = RunLimits::default();
    f(&mut l);
    l
}

#[test]
fn b776w_c9_each_member_is_checked_at_both_ends_of_its_range() {
    type Set = fn(&mut RunLimits, u64);
    let rows: [(LimitRange, LimitsProblem, Set); 6] = [
        (LIMIT_INTERRUPTS, LimitsProblem::Interrupts, |l, n| {
            l.interrupts = Some(u32::try_from(n).unwrap_or(u32::MAX))
        }),
        (LIMIT_QUESTIONS, LimitsProblem::Questions, |l, n| {
            l.questions = Some(u32::try_from(n).unwrap_or(u32::MAX))
        }),
        (
            LIMIT_FRESH_SESSIONS,
            LimitsProblem::FreshSessions,
            |l, n| l.fresh_sessions = Some(u32::try_from(n).unwrap_or(u32::MAX)),
        ),
        (
            LIMIT_RESEARCH_FILE_BYTES,
            LimitsProblem::ResearchFileBytes,
            |l, n| l.research_file_bytes = Some(n),
        ),
        (
            LIMIT_RESEARCH_UNPACKED_BYTES,
            LimitsProblem::ResearchUnpackedBytes,
            |l, n| l.research_unpacked_bytes = Some(n),
        ),
        (
            LIMIT_RESEARCH_RUN_BYTES,
            LimitsProblem::ResearchRunBytes,
            |l, n| l.research_run_bytes = Some(n),
        ),
    ];
    for (range, problem, set) in rows {
        for n in [range.min, range.default, range.max] {
            assert_eq!(
                check_limits(&with(|l| set(l, n))),
                Ok(()),
                "{problem:?} {n}"
            );
        }
        assert_eq!(
            check_limits(&with(|l| set(l, range.max + 1))),
            Err(problem),
            "{problem:?} above its range"
        );
        if range.min > 0 {
            assert_eq!(
                check_limits(&with(|l| set(l, range.min - 1))),
                Err(problem),
                "{problem:?} below its range"
            );
        }
    }
    // Zero is a word for "none of it" in the three counts and is below the minimum of the byte budgets.
    assert_eq!(check_limits(&with(|l| l.interrupts = Some(0))), Ok(()));
    assert_eq!(check_limits(&with(|l| l.questions = Some(0))), Ok(()));
    assert_eq!(check_limits(&with(|l| l.fresh_sessions = Some(0))), Ok(()));
    assert_eq!(
        check_limits(&with(|l| l.research_file_bytes = Some(0))),
        Err(LimitsProblem::ResearchFileBytes)
    );
}

#[test]
fn b776w_c9_the_first_problem_in_member_order_is_reported_and_the_message_hides_the_value() {
    let l = with(|l| {
        l.research_run_bytes = Some(9 * 1024 * MIB);
        l.questions = Some(5_000);
    });
    let p = check_limits(&l).unwrap_err();
    assert_eq!(p, LimitsProblem::Questions);
    let text = p.to_string();
    assert_eq!(p.key(), "questions");
    assert!(text.contains("questions"), "{text}");
    assert!(text.contains("0 to 1000"), "{text}");
    assert!(!text.contains("5000"), "{text}");
    assert_eq!(p.code(), "out_of_range");
}

#[test]
fn b776w_c9_the_research_budgets_are_independent_counters() {
    // A file larger than the repository budget is not an error of the member: each counter is checked alone.
    let l = with(|l| {
        l.research_file_bytes = Some(64 * MIB);
        l.research_unpacked_bytes = Some(8 * MIB);
        l.research_run_bytes = Some(4 * MIB);
    });
    assert_eq!(check_limits(&l), Ok(()));
}
