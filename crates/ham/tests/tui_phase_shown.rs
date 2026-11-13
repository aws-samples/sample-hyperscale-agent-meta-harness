//! The phase of a run on the screens of the terminal app. The service writes a run's `phase` three times (planning, green, delivering) and
//! moves the status to `complete` or `blocked` without changing it, so a finished run keeps the last phase it reached: all 769 finished runs of
//! a live account said `delivering`. A phase means something while a run is going, and for a blocked run it says where it stopped; a run
//! that is `complete` or `cleared` shows none. The Runs list draws a dash in the PHASE column, its note line under the table leaves the phase
//! out, and the run screen's status block and delivery card name the status alone. The record the service stored, `ham runs` and the JSON of
//! the API are not touched (tests `cli.rs`, `watch.rs`, `commands.rs`).
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Msg, Screen};
use ham_core::runfilter::{IndexedRead, RunsRead};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::PathBuf;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

/// Every status the Runs list can show with a phase the service stored for it: the finished statuses first, then the others.
const RUNS: [(&str, &str, Option<&str>); 7] = [
    ("complete", "delivering", None),
    ("cleared", "green", None),
    ("active", "green", Some("green")),
    ("blocked", "delivering", Some("delivering")),
    ("usage_limited", "refactor", Some("refactor")),
    ("paused", "red", Some("red")),
    ("budget_limited", "evaluating", Some("evaluating")),
];

fn id_of(i: usize) -> String {
    format!("01M5A000000000000000000R0{i}")
}

fn summary(i: usize, status: &str, phase: &str) -> RunSummary {
    serde_json::from_value(json!({"runId": id_of(i), "status": status, "phase": phase,
        "target": {"mode": "s3-only"}, "createdAt": "2026-10-20T12:00:00Z", "ownerPrincipalArn": ARN}))
    .unwrap()
}

fn runs_app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let runs = RUNS
        .iter()
        .enumerate()
        .map(|(i, (status, phase, _))| summary(i, status, phase))
        .collect();
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index: vec![],
    })));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn rows(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            line.trim_end().to_string()
        })
        .collect()
}

fn screen(app: &App, w: u16, h: u16) -> String {
    rows(app, w, h).join("\n")
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn the_phase_column_of_the_runs_list_is_a_dash_for_a_finished_run_and_the_phase_for_the_others() {
    let app = runs_app();
    // 160 columns: RUN, STATUS, NAME, TARGET, PHASE and CREATED are all columns.
    let lines = rows(&app, 160, 30);
    assert!(
        lines.iter().any(|l| l.contains("PHASE")),
        "the column exists at 160 columns:\n{}",
        lines.join("\n")
    );
    for (i, (status, stored, shown)) in RUNS.iter().enumerate() {
        let id = id_of(i);
        let line = lines.iter().find(|l| l.contains(&id)).unwrap();
        // After the id: the status, the NAME cell (a dash, no name was written), the target, the phase and the creation time.
        let after: Vec<&str> = line
            .split(&id)
            .nth(1)
            .unwrap()
            .trim_end_matches('│')
            .split_whitespace()
            .collect();
        assert_eq!(
            after,
            [
                *status,
                "-",
                "s3-only",
                shown.unwrap_or("-"),
                "2026-10-20T12:00:00Z"
            ],
            "a {status} run the service stored as {stored}: {line}"
        );
    }
}

#[test]
fn the_note_under_the_table_names_the_phase_of_the_selected_run_only_while_it_means_something() {
    let mut app = runs_app();
    for (i, (status, _stored, shown)) in RUNS.iter().enumerate() {
        // 80 columns hides PHASE, TARGET and CREATED from the table: the note under it carries them for the selected run.
        let lines = rows(&app, 80, 24);
        let note = &lines[22];
        assert!(note.contains("target s3-only"), "{status}: {note}");
        match shown {
            Some(word) => assert!(
                note.contains(&format!("phase {word}")),
                "a {status} run says where it is: {note}"
            ),
            None => assert!(
                !note.contains("phase"),
                "a {status} run says nothing about a phase: {note}"
            ),
        }
        assert!(
            lines.iter().any(|l| l.contains(&format!("▶ {}", id_of(i)))),
            "run {i} is the selected one"
        );
        press(&mut app, KeyCode::Down);
    }
}

fn env(n: u32, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
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

/// A run screen whose last `goal.status` carries `status` and `phase`.
fn run_screen(status: &str, phase: &str) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(vec![
        env(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            3,
            EventType::GoalStatus,
            json!({"status": status, "phase": phase}),
        ),
    ]));
    app
}

/// The first line under the `status` notice of the run screen: the status block's first row.
fn status_row(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|l| l.contains("▾ status")).unwrap();
    lines[at + 1].trim().to_string()
}

#[test]
fn the_status_block_of_the_run_screen_names_the_phase_of_a_run_that_is_not_over() {
    for (status, phase, shown) in RUNS {
        let text = screen(&run_screen(status, phase), 100, 30);
        let want = match shown {
            Some(word) => format!("{status} ({word})"),
            None => status.to_string(),
        };
        assert_eq!(status_row(&text), want, "{status} in {phase}:\n{text}");
        if shown.is_none() {
            assert!(
                !text.contains(&format!("{status} (")),
                "a {status} run names no phase anywhere:\n{text}"
            );
        }
    }
}

fn downloaded() -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(ham::pull::Pulled {
        path: PathBuf::from("/downloads/python-calc-R1-abc1234.zip"),
        artifact: ArtifactView {
            artifact_id: "01K6A00000000000000000ZP01".into(),
            kind: ArtifactKind::Zip,
            s3_uri: "s3://b/runs/R1/deliverable/python-calc-R1-abc1234.zip".into(),
            sha256: "ab".repeat(32),
            size_bytes: 2048,
        },
        url: DownloadUrlResponse {
            url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/deliverable/python-calc-R1-abc1234.zip?X-Amz-Expires=900".into(),
            expires_at: "2026-10-14T20:15:00Z".into(),
        },
        ranges: vec![(0, 2047)],
        resumed_from: 0,
    }),
    }
}

#[test]
fn the_delivery_card_of_a_finished_run_names_the_status_alone() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(vec![
        env(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            3,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
        env(
            4,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        env(
            5,
            EventType::DeliveryReady,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(
            6,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]));
    app.update(downloaded());
    let text = screen(&app, 100, 30);
    let card = text
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("Run       {RUN}")))
        .unwrap_or_else(|| panic!("the card has a Run line:\n{text}"));
    assert_eq!(card.trim(), format!("Run       {RUN} complete"), "{text}");
    assert_eq!(status_row(&text), "complete", "{text}");
    assert!(!text.contains("(delivering)"), "{text}");
}
