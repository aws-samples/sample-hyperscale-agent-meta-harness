//! The status row under the Runs table with tags. It reads the count text, then ` · tags ` and the tags of the selected run,
//! then the columns the width hides, all on one row that the terminal clips at its edge. Tags come right after
//! the count and the words about runs that wait; a selected run with no tags adds nothing.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::KeyCode;
use ham::tui::Screen;
use tagsfx::*;

/// The first `n` characters of `text`: what a terminal `n` columns wide shows of a row that is longer.
fn clip(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

const R1_TAGS: &str =
    "project:billing-export tech:python tech:dynamodb topic:csv-export tech:csv topic:backfill";

#[test]
fn at_80_columns_the_note_reads_the_summary_then_the_tags_and_clips() {
    let mut app = design();
    ch(&mut app, 't');
    ch(&mut app, ' ');
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.screen(), Screen::Runs);
    let (note, _) = bottom_two(&screen_text(&app, 80, 24));
    assert_eq!(
        note,
        "12 of 212 runs match +project:billing-export \u{b7} tags project:billing-export tech:"
    );
    assert_eq!(note.chars().count(), 80);
}

#[test]
fn with_no_filter_the_note_reads_the_count_then_the_tags_then_the_hidden_columns() {
    let app = design();
    let counted = format!("212 runs \u{b7} tags {R1_TAGS}");
    // The table shows CREATED from 99 columns, TARGET from 124 and PHASE from 137; the note adds the ones it does not show, in the order
    // target, created, and the terminal clips the row at its edge. The runs of the design are over (`complete`), and a run that is over has
    // no phase to show (`ham_core::render::phase_shown`): the note never says `phase delivering` for them.
    let hidden = |target: bool, created: bool| -> String {
        let mut out = String::new();
        if target {
            out.push_str(" \u{b7} target s3-only");
        }
        if created {
            out.push_str(" \u{b7} created 2026-10-19T13:00:00Z");
        }
        out
    };
    for (w, text) in [
        (80u16, format!("{counted}{}", hidden(true, true))),
        (98, format!("{counted}{}", hidden(true, true))),
        (100, format!("{counted}{}", hidden(true, false))),
        (123, format!("{counted}{}", hidden(true, false))),
        (130, counted.clone()),
        (160, counted.clone()),
        (200, counted.clone()),
    ] {
        let (note, _) = bottom_two(&screen_text(&app, w, 24));
        // The screen reader trims the end of a row, and a clip can end in the blank before the next word.
        assert_eq!(note, clip(&text, usize::from(w)).trim_end(), "{w}");
    }
}

#[test]
fn a_run_that_is_not_over_keeps_its_phase_after_the_tags_and_before_the_creation_time() {
    // A run that is `active` shows its phase, so the order of the hidden columns is target, phase, created (the order the table has them).
    let a = id(1);
    let mut active = serde_json::to_value(s3_only(&a, "2026-10-20T12:01:00Z")).unwrap();
    active["status"] = serde_json::json!("active");
    active["phase"] = serde_json::json!("green");
    let app = app_with(
        vec![serde_json::from_value(active).unwrap()],
        vec![entry(&a, Some(&["tech:go"]))],
    );
    let (note, _) = bottom_two(&screen_text(&app, 80, 24));
    assert_eq!(
        note,
        clip(
            "1 runs \u{b7} tags tech:go \u{b7} target s3-only \u{b7} phase green \u{b7} created 2026-10-20T12:01:00Z",
            80
        )
    );
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert_eq!(note, "1 runs \u{b7} tags tech:go");
}

#[test]
fn the_tags_are_those_of_the_selected_run_and_follow_the_selection() {
    let mut app = design();
    key(&mut app, KeyCode::Down);
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert_eq!(
        note,
        "212 runs \u{b7} tags project:billing-export tech:python tech:dynamodb topic:csv-export tech:csv"
    );
    // A run with no tags adds no words about them.
    for _ in 0..12 {
        key(&mut app, KeyCode::Down);
    }
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert_eq!(note, "212 runs");
}

#[test]
fn a_run_that_waits_keeps_its_words_before_the_tags() {
    let a = id(1);
    let b = id(2);
    let mut waiting = serde_json::to_value(s3_only(&a, "2026-10-20T12:01:00Z")).unwrap();
    waiting["needsInput"] = serde_json::json!({"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-20T13:00:00Z"});
    let app = app_with(
        vec![
            serde_json::from_value(waiting).unwrap(),
            s3_only(&b, "2026-10-20T12:00:00Z"),
        ],
        vec![entry(&a, Some(&["tech:go"])), entry(&b, Some(&[]))],
    );
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert_eq!(
        note,
        "2 runs \u{b7} 1 needs input \u{b7} needs input: Cline stopped \u{b7} tags tech:go"
    );
}

#[test]
fn an_old_backend_row_shows_the_project_its_repository_gives() {
    let app = app_with(vec![repo(&id(1), "2026-10-20T12:00:00Z", "calc")], vec![]);
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert_eq!(note, "1 runs \u{b7} tags project:calc");
}

#[test]
fn a_word_filter_gets_the_tags_too() {
    let mut app = design();
    ch(&mut app, '/');
    type_in(&mut app, "csv");
    key(&mut app, KeyCode::Enter);
    let (note, _) = bottom_two(&screen_text(&app, 200, 24));
    assert!(
        note.starts_with("6 of 212 runs match \"csv\" \u{b7} tags project:billing-export"),
        "{note:?}"
    );
}
