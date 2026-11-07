//! The Runs screen joins what the run index holds about a run (the first line of its prompt and its model) to the row, so the filter finds a
//! run by words of its prompt. The index comes with the list route's rows; a run without an
//! index item has no title and is found by its id, status, target and model only. A read of the newest page keeps the titles of the runs it
//! does not carry; a read of every page replaces them.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";

fn run(id: &str) -> RunSummary {
    serde_json::from_value(json!({"runId": id, "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN}))
    .unwrap()
}

fn entry(id: &str, title: &str) -> IndexEntry {
    IndexEntry {
        run_id: id.into(),
        title: Some(title.into()),
        model: Some("Claude Sonnet 5.5".into()),
        name: None,
        name_source: None,
        tags: None,
    }
}

fn all(index: Vec<IndexEntry>) -> Msg {
    Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![run(R1), run(R2), run(R3)]),
        index,
    }))
}

fn newest(runs: Vec<RunSummary>, index: Vec<IndexEntry>) -> Msg {
    Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::First {
            runs,
            has_more: true,
        },
        index,
    }))
}

fn screen(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(100, 24)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..24 {
        let mut line = String::new();
        let mut x = 0;
        while x < 100 {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        let effects = app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert!(effects.is_empty(), "{c:?} caused {effects:?}");
    }
}

fn listed(text: &str) -> Vec<&'static str> {
    [R1, R2, R3]
        .into_iter()
        .filter(|id| text.contains(id))
        .collect()
}

fn filtered(app: &mut App, words: &str) -> Vec<&'static str> {
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    type_in(app, words);
    listed(&screen(app))
}

fn app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(vec![run(R1), run(R2), run(R3)])));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

#[test]
fn a_word_of_the_prompt_finds_the_run_once_the_index_is_joined() {
    let mut app = app();
    app.update(all(vec![
        entry(R2, "Add a percent_change function with tests"),
        entry(R3, "Build a todo API"),
    ]));
    assert_eq!(
        filtered(&mut app, "percent_change"),
        vec![R2],
        "{}",
        screen(&app)
    );
}

#[test]
fn without_the_index_a_word_of_the_prompt_finds_nothing() {
    let mut app = app();
    assert!(filtered(&mut app, "percent_change").is_empty());
    app.update(all(vec![]));
    assert!(listed(&screen(&app)).is_empty(), "{}", screen(&app));
}

#[test]
fn the_model_of_the_index_is_found_too() {
    let mut app = app();
    app.update(all(vec![entry(R1, "Anything")]));
    assert_eq!(filtered(&mut app, "sonnet"), vec![R1]);
}

#[test]
fn a_read_of_the_newest_page_keeps_the_titles_of_the_runs_it_does_not_carry() {
    let mut app = app();
    app.update(all(vec![entry(R3, "Add a percent_change function")]));
    // The timer's page holds only the newest run, and no index entry for it.
    app.update(newest(vec![run(R1)], vec![]));
    assert_eq!(filtered(&mut app, "percent_change"), vec![R3]);
}

#[test]
fn a_new_title_replaces_the_old_one_and_a_read_of_every_page_forgets_the_rest() {
    let mut app = app();
    app.update(all(vec![entry(R2, "first words")]));
    app.update(newest(
        vec![run(R1), run(R2)],
        vec![entry(R2, "second words")],
    ));
    assert_eq!(filtered(&mut app, "second"), vec![R2]);
    app.update(all(vec![]));
    assert!(listed(&screen(&app)).is_empty(), "{}", screen(&app));
}

#[test]
fn a_failed_read_keeps_the_titles() {
    let mut app = app();
    app.update(all(vec![entry(R2, "Add a percent_change function")]));
    app.update(Msg::RunsIndexed(Err("timeout".into())));
    assert_eq!(filtered(&mut app, "percent_change"), vec![R2]);
    let _ = Effect::LoadRuns;
}
