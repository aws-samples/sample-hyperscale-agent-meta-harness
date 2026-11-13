//! The mouse wheel: with mouse reports on by default, a terminal no longer turns the wheel into arrow keys, so
//! the TUI does: a notch up or down moves the selection of the Runs list and the Artifacts list and scrolls the Dashboard exactly as
//! the arrow keys do, and does nothing on a screen where an arrow key would edit or choose.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::RunSummary;
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::target::RunTarget;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const RUNS: [&str; 3] = [
    "01K6A0000000000000000000R1",
    "01K6A0000000000000000000R2",
    "01K6A0000000000000000000R3",
];

fn summary(id: &str) -> RunSummary {
    RunSummary {
        needs_input: None,
        run_id: id.into(),
        status: GoalStatus::Active,
        phase: GoalPhase::Planning,
        target: RunTarget::S3Only,
        created_at: "2026-09-25T20:00:00Z".into(),
        owner_principal_arn: support::ME.into(),
        kind: None,
        aborted: None,
        continue_from: None,
        status_reason: None,
        resume_mode: None,
        session: None,
    }
}

fn app_with_runs() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(RUNS.iter().map(|id| summary(id)).collect())));
    app
}

fn screen_text(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..30u16)
        .map(|y| {
            (0..100u16)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn selected(app: &App) -> usize {
    let text = screen_text(app);
    RUNS.iter()
        .position(|id| text.contains(&format!("▶ {id}")))
        .unwrap()
}

#[test]
fn the_wheel_moves_the_selection_of_the_runs_list_one_row_a_notch() {
    let mut app = app_with_runs();
    assert_eq!(selected(&app), 0);
    assert!(app.handle_wheel(true).is_empty());
    assert!(app.handle_wheel(true).is_empty());
    assert_eq!(selected(&app), 2);
    // At the end it stays; going back up works.
    app.handle_wheel(true);
    assert_eq!(selected(&app), 2);
    app.handle_wheel(false);
    assert_eq!(selected(&app), 1);
}

#[test]
fn the_wheel_does_nothing_in_the_composer_and_never_quits_or_acts() {
    let mut app = app_with_runs();
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.screen(), Screen::Composer);
    let before = screen_text(&app);
    for down in [true, false, true] {
        assert!(app.handle_wheel(down).is_empty());
    }
    assert_eq!(app.screen(), Screen::Composer);
    assert_eq!(screen_text(&app), before, "the wheel changed the composer");
}

#[test]
fn the_wheel_on_the_dashboard_scrolls_without_effects() {
    let mut app = app_with_runs();
    app.open_run(RUNS[0]);
    assert_eq!(app.screen(), Screen::Dashboard);
    for down in [true, true, false] {
        assert_eq!(app.handle_wheel(down), Vec::<Effect>::new());
    }
    assert_eq!(app.screen(), Screen::Dashboard);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wheel_reports_through_the_event_loop_move_the_selection() {
    let fake = Fake::start().await;
    let api = Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    // Ten runs: a notch moves the selection by the 3 lines it carries (the Runs list scrolls, package FIX-RUNS-SCROLL of 2026-10-21), so three runs would end at the first.
    let ids: Vec<String> = (0..10)
        .map(|i| format!("01K6A00000000000000000{i:04}"))
        .collect();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(ids.iter().map(|id| summary(id)).collect())));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let wheel = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        })
    };
    tx.send(wheel(MouseEventKind::ScrollDown)).unwrap();
    tx.send(wheel(MouseEventKind::ScrollDown)).unwrap();
    tx.send(wheel(MouseEventKind::ScrollUp)).unwrap();
    // Moved and clicks are asked for by the session but change nothing yet.
    tx.send(wheel(MouseEventKind::Moved)).unwrap();
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))
    .unwrap();
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let dir = support::scratch("tui-wheel-loop");
    ham::tui::event_loop(
        &mut term,
        api,
        dir,
        app,
        vec![],
        rx,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..30u16)
        .map(|y| {
            (0..100u16)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Down and Down go 3 lines each, Up goes 3 back: the fourth run.
    assert!(text.contains(&format!("▶ {}", ids[3])), "{text}");
}
