//! The open-anything box with run names: a run is listed by its name when it has one, else by the title of its prompt (a backend
//! without names), and the words typed match the name as well as the title. A chat hit for a held run is listed under the run's name.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::{Clock, screen as shot, type_text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::Done;
use ham::tui::{App, DirectSettings, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use ham_core::runsearch::SearchResponse;
use harness_protocol::api::v1::RunSummary;
use serde_json::json;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";

const NAME: &str = "Order list pagination with cursors";
const TITLE1: &str = "Build the paging for the orders screen";
const TITLE2: &str = "Fix the login redirect loop on old backend";

fn run(id: &str) -> RunSummary {
    serde_json::from_value(json!({"runId": id, "status": "complete", "phase": "planning",
        "target": {"mode": "repo", "repo": "cc:calc-service"}, "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN}))
    .unwrap()
}

fn entry(id: &str, title: &str, name: Option<&str>, source: Option<&str>) -> IndexEntry {
    IndexEntry {
        run_id: id.into(),
        title: Some(title.into()),
        model: None,
        name: name.map(str::to_string),
        name_source: source.map(str::to_string),
        tags: None,
    }
}

fn app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![run(R1), run(R2), run(R3)]),
        index: vec![
            entry(R1, TITLE1, Some(NAME), Some("ai")),
            // An older backend: a title and no name.
            entry(R2, TITLE2, None, None),
            entry(R3, "Plain seed run", Some("Plain seed run"), Some("seed")),
        ],
    })));
    app
}

fn open_box(app: &mut App, clock: &mut Clock) {
    let at = clock.after(300);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), at);
}

#[test]
fn the_box_lists_a_run_by_its_name_and_an_older_backend_s_run_by_its_title() {
    let mut app = app();
    let mut clock = Clock::new();
    open_box(&mut app, &mut clock);
    let text = shot(&app, 120, 30);
    assert!(text.contains(NAME), "{text}");
    assert!(
        !text.contains(TITLE1),
        "the name stands in place of the title: {text}"
    );
    assert!(text.contains(TITLE2), "{text}");
    assert!(text.contains("Plain seed run"), "{text}");
}

#[test]
fn a_word_that_is_only_in_the_name_finds_the_run() {
    let mut app = app();
    let mut clock = Clock::new();
    open_box(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "cursors");
    let text = shot(&app, 120, 30);
    assert!(text.contains(R1), "{text}");
    assert!(!text.contains(R2) && !text.contains(R3), "{text}");
    // A word of the title still finds the run too.
    let mut app = self::app();
    let mut clock = Clock::new();
    open_box(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "paging");
    let text = shot(&app, 120, 30);
    assert!(text.contains(R1) && !text.contains(R2), "{text}");
}

#[test]
fn a_chat_hit_for_a_held_run_is_listed_under_the_run_s_name() {
    let mut app = app();
    let mut clock = Clock::new();
    open_box(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "percent");
    let r: SearchResponse = serde_json::from_value(json!({"hits": [
        {"runId": R1, "createdAt": "2026-10-16T08:00:00Z", "field": "chat",
         "snippet": "added the percent_change helper", "seq": 3, "offset": 5, "open": "open-token-7", "coverage": "full"}],
        "searched": 3, "partial": false, "more": false, "partialCoverage": 0}))
    .unwrap();
    app.update(Msg::Ext(Done::Searched {
        query: "percent".into(),
        result: Ok(r),
    }));
    let text = shot(&app, 120, 30);
    assert!(text.contains(NAME), "{text}");
    assert!(text.contains("added the percent_change helper"), "{text}");
    assert!(!text.contains(TITLE1), "{text}");
}
