//! The Runs screen joins the tags of the run index to its rows: the filter box finds a run by a word of one of its
//! tags, the entry of the newest read replaces the tags the screen held for the run, an empty list from the server is an answer (the
//! project of the repository does not come back), and a server that sends no tags leaves the row with its derived project.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Msg, Screen};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const R1: &str = "01M5A00000000000000000T001";
const R2: &str = "01M5A00000000000000000T002";

fn run(id: &str, created: &str) -> RunSummary {
    let target: Value = json!({"mode": "repo", "repo": "cc:calc"});
    serde_json::from_value(
        json!({"runId": id, "status": "complete", "phase": "planning", "target": target,
        "createdAt": created, "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

fn runs() -> Vec<RunSummary> {
    vec![
        run(R2, "2026-10-20T12:01:00Z"),
        run(R1, "2026-10-20T12:00:00Z"),
    ]
}

fn entry(id: &str, tags: Option<&[&str]>) -> IndexEntry {
    IndexEntry {
        run_id: id.into(),
        title: Some(format!("Prompt of {id}")),
        model: None,
        name: None,
        name_source: None,
        tags: tags.map(|l| l.iter().map(|t| (*t).to_string()).collect()),
    }
}

fn read(app: &mut App, read: RunsRead, index: Vec<IndexEntry>) {
    app.update(Msg::RunsIndexed(Ok(IndexedRead { read, index })));
}

fn open(index: Vec<IndexEntry>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    read(&mut app, RunsRead::All(runs()), index);
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

/// The text of the whole screen at 100x24.
fn screen(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(100, 24)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..24)
        .map(|y| {
            (0..100)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which of the two runs the filter shows.
fn shown(app: &App) -> (bool, bool) {
    let text = screen(app);
    (text.contains(R1), text.contains(R2))
}

#[test]
fn a_word_of_a_tag_finds_the_run_and_the_newest_read_replaces_the_tags() {
    let mut a = open(vec![
        entry(R1, Some(&["topic:alpha-work"])),
        entry(R2, Some(&["topic:beta-work"])),
    ]);
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "alpha");
    assert_eq!(shown(&a), (true, false), "{}", screen(&a));
    // The timer's read of the newest page brings new tags for both runs; the filter text stays.
    read(
        &mut a,
        RunsRead::First {
            runs: runs(),
            has_more: false,
        },
        vec![
            entry(R1, Some(&["topic:gamma-work"])),
            entry(R2, Some(&["topic:alpha-work"])),
        ],
    );
    assert_eq!(
        shown(&a),
        (false, true),
        "the old tags of R1 are gone, R2 now holds the word: {}",
        screen(&a)
    );
    key(&mut a, KeyCode::Esc);
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "gamma");
    assert_eq!(shown(&a), (true, false), "{}", screen(&a));
}

#[test]
fn an_empty_list_is_an_answer_and_no_list_leaves_the_derived_project() {
    // Both runs are on the repository cc:calc, so both show `project:calc` until a server says otherwise.
    let mut a = open(vec![entry(R1, None), entry(R2, None)]);
    key(&mut a, KeyCode::Char('/'));
    type_in(&mut a, "project:calc");
    assert_eq!(shown(&a), (true, true), "{}", screen(&a));
    // The person removed the project from R1: the server sends the list without it, and the screen does not add it back.
    read(
        &mut a,
        RunsRead::First {
            runs: runs(),
            has_more: false,
        },
        vec![entry(R1, Some(&["topic:keep"])), entry(R2, None)],
    );
    assert_eq!(shown(&a), (false, true), "{}", screen(&a));
    // An empty list from the server is not "no list".
    read(
        &mut a,
        RunsRead::First {
            runs: runs(),
            has_more: false,
        },
        vec![entry(R1, Some(&[])), entry(R2, Some(&[]))],
    );
    assert_eq!(shown(&a), (false, false), "{}", screen(&a));
}
