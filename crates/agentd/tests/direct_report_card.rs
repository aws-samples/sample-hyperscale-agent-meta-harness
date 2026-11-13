//! In the worker: the clean-checkout test report tells the event stream what it is doing, records the time limit each command was given,
//! and takes the limit from the request when the user set one. The whole agent runs in process against the fake AWS and the fake ACP engine, as
//! in `direct_report.rs`; the events are the `turn.progress` events of kind `report` (`harness_protocol::direct::ReportProgress`).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks, Limits};
use agentd::direct::report::{ReportHook, Settings};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::{
    PROGRESS_REPORT, ReportProgress, ReportProgressState, TestReport, TestReportStatus, s3 as keys,
};
use harness_protocol::envelope::EventType;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn settings(budget_s: u64, step_s: u64) -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(budget_s),
        step: Duration::from_secs(step_s),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

async fn run_with(d: &Direct, s: Settings, limits: Limits) {
    let hooks =
        Hooks::new(vec![Arc::new(ReportHook::new(s)) as Arc<dyn DirectHook>]).with_limits(limits);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
}

fn report_of(d: &Direct) -> Option<TestReport> {
    let bytes = d.aws.get(&keys::test_report_key(RUN))?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

/// The payloads of the report events, in the order agentd wrote them.
fn progress_of(d: &Direct) -> Vec<ReportProgress> {
    d.events_of(EventType::TurnProgress)
        .iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some(PROGRESS_REPORT))
        .map(|v| ReportProgress::from_value(v).unwrap_or_else(|| panic!("not a report event: {v}")))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_says_when_it_begins_what_command_runs_and_when_it_is_over() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(60, 30), Limits::default()).await;
    let p = progress_of(&d);
    let states: Vec<ReportProgressState> = p.iter().map(|e| e.state).collect();
    assert_eq!(
        states,
        [
            ReportProgressState::Checkout,
            ReportProgressState::Running,
            ReportProgressState::Ended
        ],
        "{p:?}"
    );
    let running = &p[1];
    assert_eq!((running.step, running.of), (Some(1), Some(1)));
    assert_eq!(
        running.command.as_deref(),
        Some("echo \"4 passed in 0.02s\"")
    );
    assert_eq!(running.limit_s, Some(30));
    assert!(
        running.text.contains("command 1 of 1") && running.text.contains("up to 30 s"),
        "{}",
        running.text
    );
    assert_eq!(p[2].status, Some(TestReportStatus::Ran));
    assert!(p.iter().all(|e| !e.text.is_empty()), "{p:?}");
    // Words about what the harness does, never about how the tests went.
    for e in &p {
        assert_eq!(
            harness_protocol::direct::banned_claim_word(&e.text),
            None,
            "{}",
            e.text
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_events_come_before_the_end_of_the_turn_and_after_the_work() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(60, 30), Limits::default()).await;
    let kinds: Vec<(EventType, String)> = d
        .aws
        .events()
        .into_iter()
        .map(|(t, v)| (t, v["kind"].as_str().unwrap_or_default().to_string()))
        .collect();
    let first = kinds
        .iter()
        .position(|(_, k)| k == PROGRESS_REPORT)
        .unwrap();
    let last = kinds
        .iter()
        .rposition(|(_, k)| k == PROGRESS_REPORT)
        .unwrap();
    let ended = kinds.iter().position(|(t, _)| *t == EventType::TurnEnded);
    if let Some(ended) = ended {
        assert!(
            last < ended,
            "the report events come before turn.ended: {kinds:?}"
        );
    }
    assert!(
        first > 0,
        "the report begins after the run began: {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_command_records_the_limit_it_was_given() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(60, 30), Limits::default()).await;
    let r = report_of(&d).unwrap();
    assert_eq!(r.steps[0].limit_s, Some(30), "{r:?}");
    assert_eq!(r.summary().timed_out, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_given_what_is_left_of_a_short_budget_records_the_shorter_limit() {
    // The stage limit is 300 s and the whole budget 6 s: the command is stopped at the budget, and the limit it records is the one it had.
    let d = Direct::start(Opts {
        scenario: "report_slow",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(6, 300), Limits::default()).await;
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::TimedOut, "{r:?}");
    let limit = r.steps[0].limit_s.expect("a command that ran has a limit");
    assert!((1..=6).contains(&limit), "{limit}");
    assert_eq!(r.summary().timed_out.unwrap().after_s, limit);
    let p = progress_of(&d);
    assert_eq!(p[1].limit_s, Some(limit), "{p:?}");
    assert_eq!(p.last().unwrap().status, Some(TestReportStatus::TimedOut));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_in_the_request_replaces_the_limit_of_the_stage() {
    // `sleep 120` with a stage limit of 30 s and a request limit of 2 s: the command is stopped after 2 seconds.
    let d = Direct::start(Opts {
        scenario: "report_slow",
        report_step_s: Some(2),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let t = Instant::now();
    run_with(&d, settings(40, 30), Limits::default()).await;
    assert!(t.elapsed() < Duration::from_secs(25), "{:?}", t.elapsed());
    let r = report_of(&d).unwrap();
    assert_eq!(r.status, TestReportStatus::TimedOut);
    assert_eq!(r.steps[0].limit_s, Some(2), "{r:?}");
    assert!(r.steps[0].duration_ms < 6_000, "{}", r.steps[0].duration_ms);
    assert_eq!(
        r.summary().sentence(),
        "`sleep 120` timed out after 2 s; the commands that finished are listed in the report."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_limit_longer_than_the_drivers_ceiling_is_honoured_and_a_stage_limit_is_not_lengthened()
 {
    // The driver's ceiling for the report step is 2 s here. A request that sets a limit (5 s) makes the module ask for the time its budget needs,
    // so the report is made; the same run without the member is cut off by the driver and makes none. (TRIGGER PRESENT: the second half.)
    let limits = Limits {
        report: Duration::from_secs(2),
        ..Limits::default()
    };
    let with = Direct::start(Opts {
        scenario: "report_slow",
        report_step_s: Some(5),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&with, settings(6, 3), limits).await;
    let r = report_of(&with).expect("the request's limit gives the report the time it needs");
    assert_eq!(r.steps[0].limit_s, Some(5), "{r:?}");
    assert!(r.steps[0].timed_out);

    let without = Direct::start(Opts {
        scenario: "report_slow",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&without, settings(6, 3), limits).await;
    assert!(
        report_of(&without).is_none(),
        "TRIGGER PRESENT: without the member the driver's 2 s ceiling cuts the report off"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_report_that_is_off_writes_no_report_events() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        report_tests: Some(false),
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(60, 30), Limits::default()).await;
    assert!(progress_of(&d).is_empty());
    assert!(report_of(&d).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tree_with_no_test_command_still_ends_the_report_in_the_events() {
    let d = Direct::start(Opts {
        scenario: "report_none",
        timeout_s: 3600,
        repo_moved: vec![("tests/test_calc.py", "tests/calc_check.py")],
        ..Opts::default()
    })
    .await;
    run_with(&d, settings(60, 30), Limits::default()).await;
    let p = progress_of(&d);
    let states: Vec<ReportProgressState> = p.iter().map(|e| e.state).collect();
    assert_eq!(
        states,
        [ReportProgressState::Checkout, ReportProgressState::Ended],
        "{p:?}"
    );
    assert_eq!(p[1].status, Some(TestReportStatus::NoCommand));
}

// ---- what the progress events carry (reviewer round on 8b335234) ------------------------------------------------------------

mod progress_events {
    use agentd::direct::report::progress::{Progress, limit_seconds, limit_words};
    use agentd::events::EventRecorder;
    use harness_protocol::direct::{ReportProgress, ReportProgressState};
    use harness_protocol::envelope::Payload;
    use std::time::Duration;

    /// The one event `Progress::running` wrote, read back as the client reads it.
    fn running_event(command: &str) -> (ReportProgress, String) {
        let rec = EventRecorder::new("run-1", "direct");
        assert_eq!(
            Progress::new(rec.clone()).running(1, 2, command, Duration::from_secs(30)),
            30
        );
        let events = rec.drain();
        assert_eq!(events.len(), 1, "{events:?}");
        let Payload::Inline { value } = &events[0].payload else {
            panic!("a report event is inline: {:?}", events[0].payload);
        };
        let p = ReportProgress::from_value(value).expect("a report event");
        assert_eq!(p.state, ReportProgressState::Running);
        (p, value.to_string())
    }

    #[test]
    fn the_command_in_a_running_event_has_its_secrets_taken_out_first() {
        // Built at run time so no key-shaped or token-shaped literal sits in the repository.
        let key = ["AK", "IA", "TESTKEY", "TESTKEY", "12"].concat();
        let token = ["tok", "en-", "abcdefgh", "12345678"].concat();
        let command = format!("AWS_KEY={key} python3 -m pytest --token={token} -q");
        let (p, whole) = running_event(&command);
        let shown = p.command.expect("the running event names the command");
        assert!(!shown.contains(&key) && !shown.contains(&token), "{shown}");
        assert!(
            !p.text.contains(&key)
                && !p.text.contains(&token)
                && !whole.contains(&key)
                && !whole.contains(&token),
            "no member of the event holds a secret: {whole}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(
            shown.contains("python3 -m pytest") && shown.ends_with("-q"),
            "the rest of the command stays: {shown}"
        );
    }

    #[test]
    fn a_command_without_a_secret_is_sent_as_it_is() {
        let (p, _) = running_event("cd scripts/tests && python3 -m unittest test_docs_set");
        assert_eq!(
            p.command.as_deref(),
            Some("cd scripts/tests && python3 -m unittest test_docs_set")
        );
    }

    #[test]
    fn a_limit_is_whole_seconds_rounded_up_and_stays_within_what_the_report_accepts() {
        for (limit, seconds) in [
            (Duration::from_millis(1_500), 2),
            (Duration::from_millis(1_000), 1),
            (Duration::from_nanos(1), 1),
            (Duration::ZERO, 1),
            (Duration::from_secs(300), 300),
            (Duration::from_millis(299_001), 300),
            (Duration::from_secs(3_600), 3_600),
            (Duration::from_secs(5_000), 5_000),
            (Duration::from_secs(27_900), 27_900),
            (Duration::from_millis(27_900_001), 27_900),
            (Duration::from_secs(40_000), 27_900),
            (Duration::MAX, 27_900),
        ] {
            assert_eq!(limit_seconds(limit), seconds, "{limit:?}");
        }
    }

    #[test]
    fn a_limit_is_worded_in_whole_minutes_or_in_seconds() {
        for (seconds, words) in [
            (1, "1 s"),
            (59, "59 s"),
            (60, "1 min"),
            (90, "90 s"),
            (143, "143 s"),
            (300, "5 min"),
            (3_600, "60 min"),
        ] {
            assert_eq!(limit_words(seconds), words);
        }
    }
}
