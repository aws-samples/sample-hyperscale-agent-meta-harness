//! On the screens of `ham`: what a person sees after Cline ends and the harness runs the repository's tests.
//!
//! The terminal showed `active (green)` and a frozen live row for five minutes, and the final card had no Tests line although the zip held
//! `evidence/test-report.json` with status `timed_out`. `ham pull` read the zip and printed the line; the card of the terminal app and the
//! card of `ham watch` never read it. These tests use that run's report as the fixture
//! (`crates/harness-protocol/tests/fixtures/dogfood-t3-test-report.json`).
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::Api;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Msg};
use ham::watch::{WatchOptions, watch};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage};
use harness_protocol::direct::{DirectOutcome, ReportProgress, banned_claim_word};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const T3: &[u8] =
    include_bytes!("../../harness-protocol/tests/fixtures/dogfood-t3-test-report.json");
const RUN: &str = "01M5HZD83QRNP7H7NN9S4KYJK2";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const SLOW: &str = "cd scripts/tests && python3 -m unittest test_docs_set test_docs_setup test_dox_lint test_internal_names test_password_scan test_lifecycle_goal_card test_lifecycle_pack test_lifecycle_lock test_lifecycle_check test_remote_job test_aws_target";
const SENTENCE: &str =
    "timed out after 300 s; the commands that finished are listed in the report.";

/// A delivered zip: `(path under the top directory, body)` entries.
fn zip_with(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(format!("calc-{RUN}-20f65bb/{name}"), opts)
            .unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn t3_zip() -> Vec<u8> {
    zip_with(&[
        ("src/a.py", b"x".to_vec()),
        ("evidence/test-report.json", T3.to_vec()),
    ])
}

/// The screen as one string of words: the card's borders and the line breaks of its wrapping do not matter to what is said.
fn words(text: &str) -> String {
    text.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- the terminal app --------------------------------------------------------------------------------------------------------

fn env(n: u32, t: EventType, authority: Authority, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: u64::from(n),
        ts: "2026-10-22T14:58:30Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: v },
    }
}

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn started() -> Vec<Envelope> {
    vec![
        env(
            1,
            EventType::RunCreated,
            Authority::Harness,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            Authority::Harness,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            3,
            EventType::GoalStatus,
            Authority::Harness,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

fn finished() -> Vec<Envelope> {
    let mut e = started();
    e.extend([
        env(10, EventType::GoalStatus, Authority::Harness, json!({"status": "complete", "phase": "delivering"})),
        env(
            11,
            EventType::DeliveryReady,
            Authority::Harness,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(
            12,
            EventType::RunCompleted,
            Authority::Harness,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]);
    e
}

fn downloaded(path: PathBuf) -> Msg {
    // The answer names its run: the card of the run on screen takes it.
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(ham::pull::Pulled {
            path,
            artifact: ArtifactView {
                artifact_id: "01K6A00000000000000000ZP01".into(),
                kind: ArtifactKind::Zip,
                s3_uri: format!("s3://b/runs/{RUN}/deliverable/calc.zip"),
                sha256: "ab".repeat(32),
                size_bytes: 2048,
            },
            url: DownloadUrlResponse {
                url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/calc.zip?X-Amz-Expires=900"
                    .into(),
                expires_at: "2026-10-22T20:15:00Z".into(),
            },
            ranges: vec![(0, 2047)],
            resumed_from: 0,
        }),
    }
}

fn delivered_app(zip: &[u8], label: &str) -> App {
    let dir = support::scratch(label);
    let path = dir.join("calc.zip");
    std::fs::write(&path, zip).unwrap();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(finished()));
    app.update(downloaded(path));
    app
}

#[test]
fn the_delivery_card_of_the_terminal_app_has_the_tests_line_of_the_zip() {
    let app = delivered_app(&t3_zip(), "report-card-tui");
    let text = words(&screen(&app, 160, 50));
    assert!(
        text.contains(&format!("Tests `{SLOW}` {SENTENCE}")),
        "the card says what the report did:\n{}",
        screen(&app, 160, 50)
    );
    // The line sits beside the label and never changes it.
    assert!(text.contains("Cline reported done"), "{text}");
    assert_eq!(banned_claim_word(&text), None, "{text}");
}

#[test]
fn the_card_says_how_to_allow_more_time_when_a_command_was_stopped() {
    let app = delivered_app(&t3_zip(), "report-card-hint");
    let text = words(&screen(&app, 160, 50));
    assert!(
        text.contains("ham config set run.report_command_minutes 10"),
        "{text}"
    );
}

#[test]
fn a_zip_without_a_report_has_no_tests_line_and_nothing_moves() {
    let app = delivered_app(
        &zip_with(&[("src/a.py", b"x".to_vec())]),
        "report-card-none",
    );
    let text = screen(&app, 160, 50);
    assert!(!text.contains("Tests "), "{text}");
}

#[test]
fn before_the_zip_is_downloaded_there_is_no_tests_line() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(finished()));
    let text = screen(&app, 160, 50);
    assert!(!text.contains("Tests "), "{text}");
}

/// The first row under the `status` notice of the run screen: the status block's first row.
fn status_row(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|l| l.contains("▾ status")).unwrap();
    lines[at + 1].trim().to_string()
}

#[test]
fn the_header_names_the_report_while_it_runs_and_goes_back_when_it_ends() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut events = started();
    app.update(pages(events.clone()));
    assert_eq!(status_row(&screen(&app, 120, 40)), "active (green)");

    let running = ReportProgress::running(
        2,
        2,
        "python3 -m unittest",
        300,
        "Running the repository's tests: command 2 of 2 (up to 5 min).",
    );
    events.push(env(
        4,
        EventType::TurnProgress,
        Authority::Agent,
        running.to_value(),
    ));
    app.update(pages(vec![events.last().unwrap().clone()]));
    let text = screen(&app, 120, 40);
    assert_eq!(
        status_row(&text),
        "active (running the repository's tests)",
        "{text}"
    );
    assert!(
        words(&text).contains("Running the repository's tests: command 2 of 2 (up to 5 min)."),
        "the live row moves with the report:\n{text}"
    );
    assert!(!status_row(&text).contains("green"), "{text}");

    let ended = ReportProgress::ended(
        harness_protocol::direct::TestReportStatus::Ran,
        "The repository's tests ended.",
    );
    app.update(pages(vec![env(
        5,
        EventType::TurnProgress,
        Authority::Agent,
        ended.to_value(),
    )]));
    assert_eq!(status_row(&screen(&app, 120, 40)), "active (green)");
}

// ---- `ham watch` and `ham pull` --------------------------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
}

async fn new_run(fake: &Fake) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "report card", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, "k-report-card")
        .await
        .unwrap()
        .run_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_final_card_of_ham_watch_has_the_tests_line_of_the_zip_it_downloaded() {
    let fake = Fake::start().await;
    let run = new_run(&fake).await;
    fake.mem.inner.lock().unwrap().deliverable = Some(t3_zip());
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let dir = support::scratch("report-card-watch");
    let opts = WatchOptions {
        interval: Duration::from_millis(20),
        json: false,
        download_dir: Some(dir),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts, &mut out, &mut err)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(
        text.lines()
            .any(|l| l.starts_with("Tests     `cd scripts/tests") && l.contains(SENTENCE)),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l == "Outcome   Cline reported done"),
        "the line does not touch the label: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ham_watch_without_a_report_in_the_zip_prints_what_it_printed_before() {
    let fake = Fake::start().await;
    let run = new_run(&fake).await;
    fake.mem.inner.lock().unwrap().deliverable = Some(zip_with(&[("src/a.py", b"x".to_vec())]));
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let opts = WatchOptions {
        interval: Duration::from_millis(20),
        json: false,
        download_dir: Some(support::scratch("report-card-watch-none")),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts, &mut out, &mut err)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&out).contains("Tests "));
}

#[test]
fn the_pull_line_for_a_stopped_command_names_the_stop_and_the_setting() {
    let report: harness_protocol::direct::TestReport = serde_json::from_slice(T3).unwrap();
    let line = ham::report::card_line(&report.summary());
    assert_eq!(
        line,
        format!(
            "Tests     `{SLOW}` {SENTENCE} Raise the limit with `ham config set run.report_command_minutes 10`."
        )
    );
    // A report that ran to its end has the sentence alone.
    let mut ran = report;
    ran.status = harness_protocol::direct::TestReportStatus::Ran;
    ran.steps.retain(|s| !s.timed_out);
    assert!(
        !ham::report::card_line(&ran.summary()).contains("run.report_command_minutes"),
        "{}",
        ham::report::card_line(&ran.summary())
    );
}

// ---- the advice for a command that was stopped (reviewer round on 8b335234) --------------------------------------------------

fn advice_for(limit_s: u32) -> Option<u32> {
    let mut report: harness_protocol::direct::TestReport = serde_json::from_slice(T3).unwrap();
    for s in report.steps.iter_mut().filter(|s| s.timed_out) {
        s.limit_s = Some(limit_s);
    }
    let text = ham::report::tests_text(&report.summary());
    let tail = text
        .split(&format!("ham config set {} ", ham::report::LIMIT_KEY))
        .nth(1)?;
    Some(tail.trim_end_matches(['`', '.']).parse().unwrap())
}

#[test]
fn the_advice_names_twice_the_larger_of_the_limit_the_command_had_and_the_stage_default() {
    // agentd gives each command the step limit or what is left of the report's budget, whichever is less. A command stopped when the budget
    // ran out records that rest (6, 65 and 143 s were seen); the advice still has to name more than the stage's own 5 minutes, because a
    // smaller number would lower the step limit and the same stop would come again.
    for (given, minutes) in [
        (6, 10),
        (65, 10),
        (143, 10),
        (299, 10),
        (300, 10),
        (301, 11),
        (600, 20),
        (1_800, 60),
        (2_000, 67),
        (3_000, 100),
        (3_599, 120),
        (14_000, 465),
        (27_899, 465),
    ] {
        assert_eq!(
            advice_for(given),
            Some(minutes),
            "a command stopped after {given} s"
        );
    }
}

#[test]
fn the_advice_raises_the_limit_for_every_limit_a_command_can_record() {
    for given in 1..harness_protocol::direct::MAX_REPORT_STEP_S {
        let minutes = advice_for(given).unwrap_or_else(|| panic!("no advice for {given} s"));
        assert!(
            (1..=harness_protocol::direct::MAX_REPORT_STEP_S / 60).contains(&minutes)
                && minutes * 60 > given.max(harness_protocol::direct::DEFAULT_REPORT_STEP_S),
            "{given} s advises {minutes} min"
        );
    }
}

#[test]
fn a_command_stopped_at_the_largest_limit_gets_no_advice() {
    assert_eq!(
        advice_for(harness_protocol::direct::MAX_REPORT_STEP_S),
        None
    );
    let mut report: harness_protocol::direct::TestReport = serde_json::from_slice(T3).unwrap();
    for s in report.steps.iter_mut().filter(|s| s.timed_out) {
        s.limit_s = Some(harness_protocol::direct::MAX_REPORT_STEP_S);
    }
    let line = ham::report::card_line(&report.summary());
    assert!(!line.contains("run.report_command_minutes"), "{line}");
    assert!(
        line.contains("timed out after 27900 s"),
        "the sentence is kept: {line}"
    );
}

// ---- one read of the zip for `ham pull` (reviewer round on 8b335234) ----------------------------------------------------------

#[test]
fn a_card_takes_its_tests_line_from_a_summary_that_was_already_read() {
    // `ham pull` reads the zip once (`report::summary_of_zip`) and uses that one summary for the card and for the `--json` member.
    let report: harness_protocol::direct::TestReport = serde_json::from_slice(T3).unwrap();
    let summary = report.summary();
    let base = ham_core::render::FinalCard {
        local_path: "/Downloads/calc.zip".into(),
        ..Default::default()
    };
    let card = ham::report::with_summary(base.clone(), Some(&summary));
    assert_eq!(
        card.tests.as_deref(),
        Some(ham::report::tests_text(&summary).as_str())
    );
    assert_eq!(
        card.local_path, "/Downloads/calc.zip",
        "the rest of the card is kept"
    );
    // No summary, no line.
    assert_eq!(ham::report::with_summary(base, None).tests, None);
}

#[test]
fn the_tests_line_made_from_a_summary_is_safe_for_a_screen() {
    let mut report: harness_protocol::direct::TestReport = serde_json::from_slice(T3).unwrap();
    for s in report.steps.iter_mut().filter(|s| s.timed_out) {
        // The report's own sentence already drops control characters (an escape, a bell); a right-to-left override is not one, and it
        // can turn the rest of a line around on a screen, so the client's cleaning has to take it out.
        s.command = "make test\u{1b}[2J\u{202e}gnp.exe".into();
    }
    let card = ham::report::with_summary(
        ham_core::render::FinalCard::default(),
        Some(&report.summary()),
    );
    let tests = card.tests.expect("the report has a line");
    assert!(tests.contains("make test"), "{tests:?}");
    assert!(!tests.chars().any(char::is_control), "{tests:?}");
    assert!(!tests.contains('\u{202e}'), "{tests:?}");
}
