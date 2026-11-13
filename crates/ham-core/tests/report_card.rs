//! On the client: the setting `run.report_command_minutes` and the request member `reportStepS` it becomes, the progress of the
//! clean-checkout test report on the run view (a phase word, the live row and a notice per event), and the `Tests` line of the delivery card.
//!
//! The events are the payloads `harness_protocol::direct::ReportProgress` builds, the type agentd writes them with.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, ConfigError, RunTable, get_key, render_config, set_key, unset_key};
use ham_core::notice::{NoticeKind, notice_of};
use ham_core::render::{FinalCard, final_card, status_line};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::settings::{Key, ROWS};
use ham_core::state::{RunState, RunView};
use harness_protocol::direct::{ReportProgress, TestReportStatus};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const PHASE: &str = "running the repository's tests";

// ---- the setting ------------------------------------------------------------------------------------------------------------

fn args() -> DirectArgs {
    let Command::StartDirect(a) = parse(&["fix it", "--s3-only", "--direct"]).unwrap().command
    else {
        panic!("not a direct start");
    };
    a
}

fn cfg_with(minutes: Option<u32>) -> Config {
    Config {
        run: Some(RunTable {
            report_command_minutes: minutes,
            ..RunTable::default()
        }),
        config_version: Some(1),
        ..Config::default()
    }
}

fn request_of(cfg: &Config) -> Value {
    let r = resolve_run_settings(cfg, &|_: &str| None, &args(), false).unwrap();
    serde_json::to_value(r.request(DirectInputs::default()).unwrap()).unwrap()
}

#[test]
fn the_key_is_the_seventeenth_row_of_the_registry_and_goes_on_the_wire_as_report_step_s() {
    let key = Key::lookup("run.report_command_minutes").expect("the key exists");
    // It joined the registry after the sixteen older keys; the five stall limits were appended after it.
    assert_eq!(Key::ALL.get(16), Some(&key));
    let row = key.row();
    assert_eq!(ROWS.get(16).map(|r| r.key), Some(key));
    assert_eq!(row.file, ("run", "report_command_minutes"));
    assert_eq!(row.wire, Some("reportStepS"));
    assert_eq!(row.since, 1);
    assert_eq!(row.kind.type_text(), "whole minutes, 0 to 465");
    assert!(row.default_text.contains("5"), "{}", row.default_text);
    assert_eq!(Key::lookup("run.report_command_minutes"), Some(key));
}

#[test]
fn set_get_and_unset_keep_the_key_in_the_run_table_and_write_the_config_version() {
    let cfg = set_key(&Config::default(), "run.report_command_minutes", "6").unwrap();
    assert_eq!(
        get_key(&cfg, "run.report_command_minutes")
            .unwrap()
            .as_deref(),
        Some("6")
    );
    assert_eq!(cfg.config_version, Some(1));
    let text = render_config(&cfg);
    assert!(text.contains("[run]"), "{text}");
    assert!(text.contains("report_command_minutes = 6"), "{text}");
    let back = unset_key(&cfg, "run.report_command_minutes").unwrap();
    assert_eq!(back, Config::default());
}

#[test]
fn a_value_over_the_session_bound_is_refused_in_words_that_fit_this_key() {
    let err = set_key(&Config::default(), "run.report_command_minutes", "466").unwrap_err();
    let ConfigError::Invalid { reason, .. } = err else {
        panic!("{err:?}");
    };
    assert_eq!(
        reason,
        "takes whole minutes from 0 (use the stage default) to 465, got \"466\""
    );
    // The answer wait keeps its own words.
    let err = set_key(&Config::default(), "input_wait_minutes", "43201").unwrap_err();
    assert!(
        err.to_string().contains("0 (do not wait) to 43200"),
        "{err}"
    );
}

#[test]
fn minutes_become_seconds_on_the_request_and_zero_or_unset_send_nothing() {
    assert_eq!(request_of(&cfg_with(Some(6)))["reportStepS"], 360);
    assert_eq!(request_of(&cfg_with(Some(60)))["reportStepS"], 3600);
    for none in [Some(0), None] {
        let v = request_of(&cfg_with(none));
        assert!(v.get("reportStepS").is_none(), "{none:?}: {v}");
    }
    assert!(request_of(&Config::default()).get("reportStepS").is_none());
}

#[test]
fn the_origin_of_the_key_is_the_file_or_the_default() {
    let key = Key::lookup("run.report_command_minutes").unwrap();
    let set = resolve_run_settings(&cfg_with(Some(6)), &|_: &str| None, &args(), false).unwrap();
    assert_eq!(set.origin(key), Some(Origin::File));
    let unset = resolve_run_settings(&Config::default(), &|_: &str| None, &args(), false).unwrap();
    assert_eq!(unset.origin(key), Some(Origin::Default));
}

// ---- the progress of the report on the run view -----------------------------------------------------------------------------

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, authority: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-22T14:58:30Z",
        "type": ty, "authority": authority, "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn progress(n: u32, p: &ReportProgress) -> Envelope {
    event(n, "turn.progress", "agent", p.to_value())
}

fn goal(n: u32, status: &str, phase: &str) -> Envelope {
    event(
        n,
        "goal.status",
        "harness",
        json!({"status": status, "phase": phase}),
    )
}

fn base() -> Vec<Envelope> {
    vec![
        event(
            1,
            "run.created",
            "harness",
            json!({"target": {"mode": "s3-only"}}),
        ),
        event(
            2,
            "session.started",
            "harness",
            json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5"}),
        ),
        goal(3, "active", "green"),
    ]
}

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

fn running(n: u32) -> Envelope {
    progress(
        n,
        &ReportProgress::running(
            2,
            2,
            "python3 -m unittest",
            300,
            "Running the repository's tests: command 2 of 2 (up to 5 min).",
        ),
    )
}

#[test]
fn while_the_report_runs_the_phase_says_so_instead_of_green() {
    let mut events = base();
    assert_eq!(view_of(&events).phase_now(), Some("green"));
    events.push(progress(
        4,
        &ReportProgress::checkout("Making a clean checkout to run the repository's tests."),
    ));
    assert_eq!(view_of(&events).phase_now(), Some(PHASE));
    events.push(running(5));
    let v = view_of(&events);
    assert_eq!(v.phase_now(), Some(PHASE));
    let r = v
        .report
        .as_ref()
        .expect("the view keeps the report's state");
    assert_eq!((r.step, r.of, r.limit_s), (Some(2), Some(2), Some(300)));
    // The stored phase is what the service wrote: the record is not rewritten.
    assert_eq!(v.phase, Some(harness_protocol::goal::GoalPhase::Green));
}

#[test]
fn the_phase_goes_back_when_the_report_ends_when_delivery_begins_or_when_the_run_is_over() {
    let mut events = base();
    events.push(running(4));
    assert_eq!(view_of(&events).phase_now(), Some(PHASE));

    let mut ended = events.clone();
    ended.push(progress(
        5,
        &ReportProgress::ended(TestReportStatus::Ran, "The repository's tests ended."),
    ));
    assert_eq!(view_of(&ended).phase_now(), Some("green"));

    let mut delivering = events.clone();
    delivering.push(goal(5, "active", "delivering"));
    assert_eq!(view_of(&delivering).phase_now(), Some("delivering"));
    // A report event that sorts after the phase change does not bring the words back.
    delivering.push(running(6));
    assert_eq!(view_of(&delivering).phase_now(), Some("delivering"));

    let mut over = events.clone();
    over.push(goal(5, "complete", "delivering"));
    assert_eq!(view_of(&over).phase_now(), None);

    let mut blocked = events;
    blocked.push(goal(5, "blocked", "green"));
    assert_eq!(view_of(&blocked).phase_now(), Some("green"));
}

#[test]
fn a_state_this_client_does_not_know_changes_nothing_on_the_screen() {
    let mut events = base();
    events.push(event(
        4,
        "turn.progress",
        "agent",
        json!({"kind": "report", "state": "paused", "text": "later version"}),
    ));
    let v = view_of(&events);
    assert_eq!(v.phase_now(), Some("green"));
    assert!(v.report.is_none());
}

#[test]
fn the_live_row_shows_the_text_of_the_event() {
    let mut events = base();
    events.push(running(4));
    let v = view_of(&events);
    assert_eq!(
        v.agents["direct"].last_summary,
        "Running the repository's tests: command 2 of 2 (up to 5 min)."
    );
}

#[test]
fn each_report_event_is_one_run_notice_with_its_own_text() {
    let events = [
        progress(11, &ReportProgress::checkout("Making a clean checkout.")),
        running(12),
        progress(
            13,
            &ReportProgress::ended(
                TestReportStatus::TimedOut,
                "The repository's tests ended: a command timed out.",
            ),
        ),
    ];
    let want = [
        "Making a clean checkout.",
        "Running the repository's tests: command 2 of 2 (up to 5 min).",
        "The repository's tests ended: a command timed out.",
    ];
    for (e, text) in events.iter().zip(want) {
        let n = notice_of(e).expect("a notice");
        assert_eq!(n.kind, NoticeKind::Notify);
        assert_eq!(n.text, text);
    }
    let mut all = base();
    all.extend(events);
    let v = view_of(&all);
    let lines: Vec<&str> = v.notices.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(lines, want);
}

#[test]
fn text_from_the_event_is_made_safe_before_it_reaches_the_screen() {
    let mut events = base();
    events.push(event(
        4,
        "turn.progress",
        "agent",
        json!({"kind": "report", "state": "running", "step": 1, "of": 1, "limitS": 300,
               "command": "make\u{1b}[31m test", "text": "tests\u{1b}[2J\nnext line"}),
    ));
    let v = view_of(&events);
    let live = &v.agents["direct"].last_summary;
    let notice = &v.notices[0].text;
    let command = v.report.unwrap().command.unwrap_or_default();
    for (what, text) in [
        ("live row", live),
        ("notice", notice),
        ("command", &command),
    ] {
        assert!(!text.chars().any(char::is_control), "{what}: {text:?}");
    }
    assert!(live.starts_with("tests"), "{live:?}");
}

#[test]
fn the_status_line_names_the_report_while_it_runs() {
    let mut events = base();
    let plain = status_line(&view_of(&events));
    assert!(plain.contains("active (green)"), "{plain}");
    events.push(running(4));
    let line = status_line(&view_of(&events));
    assert!(line.contains(&format!("active ({PHASE})")), "{line}");
    assert!(!line.contains("(green)"), "{line}");
}

// ---- the Tests line of the card ---------------------------------------------------------------------------------------------

fn finished() -> RunView {
    let mut events = base();
    events.push(goal(4, "complete", "delivering"));
    view_of(&events)
}

#[test]
fn the_card_has_a_tests_line_only_when_it_is_given_one() {
    let v = finished();
    let none = final_card(&v, &FinalCard::default());
    assert!(!none.contains("Tests "), "{none}");
    let card = FinalCard {
        tests: Some("`make test` timed out after 300 s; the commands that finished are listed in the report.".into()),
        ..FinalCard::default()
    };
    let text = final_card(&v, &card);
    let last = text.lines().last().unwrap();
    assert_eq!(
        last,
        "Tests     `make test` timed out after 300 s; the commands that finished are listed in the report."
    );
    // Nothing else on the card moved.
    let without: Vec<&str> = none.lines().collect();
    let with: Vec<&str> = text.lines().collect();
    assert_eq!(&with[..with.len() - 1], &without[..]);
}
