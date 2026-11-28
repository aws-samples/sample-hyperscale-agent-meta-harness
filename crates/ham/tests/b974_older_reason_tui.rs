//! Why the older runs are not loaded stays readable for as long as they are not. The reason of their failed read stands on the Runs
//! screen's note line through the timer's reads, behind a note that tells the user to press `r`, behind the first screen's line about
//! the mouse and beside the words of another failed read; in the tag explorer it stands on a row of its own above the note line. A
//! read of the older runs that begins takes it away, one that fails again brings its own reason, and a list that is whole ends it.
//! These tests drive `App::update` and draw on a test terminal; no time is asserted.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::KeyCode;
use ham::tui::mascot::{self, Form, Look};
use ham::tui::{App, DirectSettings, Effect, MouseState, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use serde_json::json;
use tagsfx::*;

const LOADING: &str = "\u{250c} runs \u{b7} loading older runs \u{2500}";
const NOT_LOADED: &str = "\u{250c} runs \u{b7} older runs not loaded \u{b7} r loads all \u{2500}";
const PLAIN: &str = "\u{250c} runs \u{2500}";
const TAGS_NOT_LOADED: &str = "\u{250c} tags \u{b7} newest 100 runs only - r loads all ";
const TAGS_LOADING: &str = "\u{250c} tags \u{b7} newest 100 runs, loading the older ones ";
const NO_RUNS: &str = "no runs yet: press n to start one";
const NOT_EVERY: &str = "not every run is loaded: press r, then R";
const D_NOT_LOADED: &str = "remove needs every run loaded: press r to load them all, then d";
const HINT: &str = "mouse is on: hold Shift to select text with the terminal";
/// The sentence of a failed read of the older runs, without its reason.
const OLDER: &str = "could not load the older runs";
/// The sentence with the reason of the read that [`failed`] lets fail.
const WHY: &str = "could not load the older runs: timeout";

/// Runs `from..to`, newest first: run `n` was created `n` seconds before noon.
fn rows(from: usize, to: usize) -> Vec<RunSummary> {
    (from..to)
        .map(|n| {
            let t = 43_200 - n;
            let created = format!(
                "2026-10-20T{:02}:{:02}:{:02}Z",
                t / 3600,
                (t / 60) % 60,
                t % 60
            );
            serde_json::from_value(json!({"runId": id(n), "status": "complete", "phase": "delivering",
                "target": {"mode": "repo", "repo": "cc:calc"}, "createdAt": created, "ownerPrincipalArn": ARN}))
            .unwrap()
        })
        .collect()
}

fn tagged(from: usize, to: usize) -> Vec<IndexEntry> {
    (from..to)
        .map(|n| entry(&id(n), Some(&["topic:old"])))
        .collect()
}

fn page(runs: Vec<RunSummary>, index: Vec<IndexEntry>) -> IndexedRead {
    IndexedRead {
        read: RunsRead::First {
            runs,
            has_more: true,
        },
        index,
    }
}

/// The newest page of a whole read: more pages follow.
fn newest(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsNewest(page(runs, vec![]))
}

/// The end of a whole read: every page.
fn older(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index: vec![],
    }))
}

fn older_failed(error: &str) -> Msg {
    Msg::RunsOlder(Err(error.into()))
}

/// The timer's read of the newest page, with more behind it.
fn timer(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsIndexed(Ok(page(runs, vec![])))
}

fn app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(unix(NOW));
    app
}

fn text(app: &App) -> String {
    screen_text(app, 140, 30)
}

fn top(text: &str) -> &str {
    text.lines()
        .find(|l| l.starts_with('\u{250c}'))
        .unwrap_or("")
}

fn has_line(text: &str, start: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(start))
}

/// Whether the mascot's large picture is on the screen: every row of it that is not blank, as `mascot::plain` gives them.
fn picture(text: &str) -> bool {
    let rows: Vec<String> = mascot::plain(Form::Large, Look::Blocks)
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    assert!(rows.len() >= 2, "the large form has no rows to look for");
    rows.iter().all(|row| text.contains(row.as_str()))
}

/// The Runs screen with the newest 100 runs after a read of the older runs that failed with `timeout`.
fn failed() -> App {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    app.update(older_failed("timeout"));
    let text = text(&app);
    assert!(
        top(&text).starts_with(NOT_LOADED) && has_line(&text, WHY),
        "TRIGGER ABSENT: {text}"
    );
    app
}

#[test]
fn b974_the_reason_stays_through_the_timers_reads() {
    let mut app = failed();
    // The newest page is read again every 5 seconds. Its answer says nothing about the older runs: they are still not loaded, and why.
    for n in 1..=3 {
        app.update(timer(rows(0, 100)));
        let text = text(&app);
        assert!(top(&text).starts_with(NOT_LOADED), "read {n}: {text}");
        assert!(
            has_line(&text, WHY),
            "the timer's read {n} took the reason away: {text}"
        );
        assert!(!text.contains("cannot list runs"), "read {n}: {text}");
    }
    // The one-page read of before says as little about them.
    app.update(Msg::RunsRead(Ok(RunsRead::First {
        runs: rows(0, 100),
        has_more: true,
    })));
    let text = self::text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(has_line(&text, WHY), "{text}");
}

#[test]
fn b974_the_reason_follows_the_note_that_names_r_on_the_same_line() {
    let mut app = failed();
    assert!(ch(&mut app, 'R').is_empty());
    let both = format!("{NOT_EVERY} \u{b7} {WHY}");
    let text = text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        has_line(&text, &both),
        "the note hides why the older runs are not loaded: {text}"
    );
    // No key is pressed: the note stays, and the reason with it, through the timer's reads.
    app.update(timer(rows(0, 100)));
    app.update(timer(rows(0, 100)));
    let text = self::text(&app);
    assert!(has_line(&text, &both), "{text}");
}

#[test]
fn b974_a_narrow_terminal_shows_the_note_and_the_reason_on_rows_of_the_note() {
    let mut app = failed();
    assert!(ch(&mut app, 'R').is_empty());
    // 60 columns hold neither sentence beside the other: they wrap, and every word of both is on the screen.
    let text = screen_text(&app, 60, 30);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains(NOT_EVERY), "{text}");
    assert!(flat.contains(WHY), "{text}");
}

#[test]
fn b974_the_reason_follows_the_first_screens_line_about_the_mouse() {
    let mut app = app();
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        Some(HINT.into()),
    );
    app.update(newest(rows(0, 100)));
    app.update(older_failed("timeout"));
    let text = text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        has_line(&text, &format!("{HINT} \u{b7} {WHY}")),
        "the line about the mouse hides why the older runs are not loaded: {text}"
    );
    // The first key ends the line about the mouse; the reason stays.
    key(&mut app, KeyCode::Down);
    let text = self::text(&app);
    assert!(!text.contains(HINT), "TRIGGER ABSENT: {text}");
    assert!(has_line(&text, WHY), "{text}");
}

#[test]
fn b974_another_failed_read_speaks_beside_the_reason() {
    for (name, other) in [
        ("Msg::RunsIndexed", Msg::RunsIndexed(Err("reset".into()))),
        ("Msg::Runs", Msg::Runs(Err("reset".into()))),
        ("Msg::RunsRead", Msg::RunsRead(Err("reset".into()))),
    ] {
        let mut app = failed();
        app.update(other);
        let text = text(&app);
        assert!(top(&text).starts_with(NOT_LOADED), "{name}: {text}");
        // The other read says its own words, the older runs keep theirs, and neither takes the other's reason.
        assert!(
            has_line(&text, &format!("cannot list runs: reset \u{b7} {WHY}")),
            "{name}: {text}"
        );
        assert!(!text.contains(&format!("{OLDER}: reset")), "{name}: {text}");
        // The newest page is read again and answers: the other read's words go, the reason stays.
        app.update(timer(rows(0, 100)));
        let text = self::text(&app);
        assert!(!text.contains("cannot list runs"), "{name}: {text}");
        assert!(has_line(&text, WHY), "{name}: {text}");
    }
}

#[test]
fn b974_a_read_that_begins_takes_the_reason_away_and_one_that_fails_again_brings_its_own() {
    let mut app = failed();
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    // Until the newest page of that read lands the older runs are not loaded, and the screen still says why.
    let text = text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(has_line(&text, WHY), "{text}");
    // The newest page lands: older runs are being loaded, and the reason of the read before says nothing about this one.
    app.update(newest(rows(0, 100)));
    let text = self::text(&app);
    assert!(top(&text).starts_with(LOADING), "{text}");
    assert!(!text.contains(OLDER), "{text}");
    // The read fails again: its own reason, and only that.
    app.update(older_failed("reset twice"));
    let text = self::text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(has_line(&text, &format!("{OLDER}: reset twice")), "{text}");
    assert!(!text.contains("timeout"), "{text}");
}

#[test]
fn b974_a_list_that_is_whole_ends_the_reason() {
    let whole: [(&str, Msg); 5] = [
        ("the end of a whole read", older(rows(0, 250))),
        (
            "a whole read that is one answer",
            Msg::RunsIndexed(Ok(IndexedRead {
                read: RunsRead::All(rows(0, 250)),
                index: vec![],
            })),
        ),
        ("the read of before", Msg::Runs(Ok(rows(0, 250)))),
        (
            "every page as one read",
            Msg::RunsRead(Ok(RunsRead::All(rows(0, 250)))),
        ),
        (
            "a newest page with nothing behind it",
            Msg::RunsIndexed(Ok(IndexedRead {
                read: RunsRead::First {
                    runs: rows(0, 100),
                    has_more: false,
                },
                index: vec![],
            })),
        ),
    ];
    for (name, msg) in whole {
        let mut app = failed();
        app.update(msg);
        let text = text(&app);
        assert!(top(&text).starts_with(PLAIN), "{name}: {text}");
        assert!(!text.contains(OLDER), "{name}: {text}");
        // Nothing of it comes back with a later failed read: that read says its own words alone.
        app.update(Msg::RunsIndexed(Err("reset".into())));
        let text = self::text(&app);
        assert!(has_line(&text, "cannot list runs: reset"), "{name}: {text}");
        assert!(!text.contains(OLDER), "{name}: {text}");
    }
}

#[test]
fn b974_no_runs_yet_is_not_said_while_the_older_runs_are_not_loaded() {
    // A newest page can hold no run of the person (probe runs are left out of the list). When the older runs could not be loaded, the
    // screen cannot say that there are none, however often the newest page is read again.
    let mut app = app();
    app.update(newest(vec![]));
    app.update(older_failed("timeout"));
    for n in 0..3 {
        let text = text(&app);
        assert!(top(&text).starts_with(NOT_LOADED), "read {n}: {text}");
        assert!(!text.contains(NO_RUNS), "read {n}: {text}");
        assert!(has_line(&text, WHY), "read {n}: {text}");
        app.update(timer(vec![]));
    }
}

#[test]
fn b974_the_mascots_picture_waits_as_no_runs_yet_does_while_the_older_runs_are_not_loaded() {
    // Over an empty table the picture says what `no runs yet` says: the service has answered that there is no run. A whole list
    // with no run shows both, and the smoke under the picture moves with the timer.
    let mut none = app();
    none.update(Msg::Runs(Ok(vec![])));
    let shown = text(&none);
    assert!(
        shown.contains(NO_RUNS) && picture(&shown),
        "TRIGGER ABSENT: {shown}"
    );
    let _ = none.tick();
    assert_eq!(none.mascot_frame(), 1, "TRIGGER ABSENT: the smoke moves");
    // While the older runs could not be loaded that is not known, however often the newest page is read again: the picture waits
    // as the words do, and no smoke is counted for a picture that is not there.
    let mut app = app();
    app.update(newest(vec![]));
    app.update(older_failed("timeout"));
    for n in 0..3 {
        let shown = text(&app);
        assert!(has_line(&shown, WHY), "TRIGGER ABSENT, read {n}: {shown}");
        assert!(!shown.contains(NO_RUNS), "read {n}: {shown}");
        assert!(
            !picture(&shown),
            "read {n}: the picture stands beside the reason:\n{shown}"
        );
        let _ = app.tick();
        assert_eq!(
            app.mascot_frame(),
            0,
            "read {n}: smoke is counted for a picture that is not there"
        );
        app.update(timer(vec![]));
    }
    // The whole list lands and holds no run: the words and the picture come together.
    app.update(Msg::Runs(Ok(vec![])));
    let shown = text(&app);
    assert!(
        shown.contains(NO_RUNS) && picture(&shown) && !shown.contains(OLDER),
        "{shown}"
    );
}

#[test]
fn b974_the_tag_explorer_says_why_the_older_runs_are_not_loaded() {
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    app.update(older_failed("timeout"));
    ch(&mut app, 't');
    let text = screen(&app);
    assert!(text.contains(TAGS_NOT_LOADED), "TRIGGER ABSENT: {text}");
    // The reason stands on a row of its own above the note line; the note line and the key line are the explorer's own, as before.
    let rows_of = |text: &str| -> Vec<String> { text.lines().map(str::to_string).collect() };
    let lines = rows_of(&text);
    assert_eq!(
        lines[lines.len() - 3].trim(),
        WHY,
        "the tag explorer does not say why the older runs are not loaded: {text}"
    );
    let (note, help) = bottom_two(&text);
    assert!(!note.contains(OLDER), "{text}");
    assert!(help.contains("r reload"), "{text}");
    // Through the timer's reads.
    app.update(Msg::RunsIndexed(Ok(page(rows(0, 100), tagged(0, 100)))));
    let lines = rows_of(&screen(&app));
    assert_eq!(lines[lines.len() - 3].trim(), WHY);
    // `d` is refused with the sentence that names `r`: it stands on the note line, the reason above it.
    assert!(ch(&mut app, 'd').is_empty());
    let text = screen(&app);
    let lines = rows_of(&text);
    assert_eq!(bottom_two(&text).0, D_NOT_LOADED, "{text}");
    assert_eq!(lines[lines.len() - 3].trim(), WHY, "{text}");
    // `r` reads again: when its newest page lands, older runs are being loaded and the reason is gone.
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    let text = screen(&app);
    assert!(text.contains(TAGS_LOADING), "{text}");
    assert!(!text.contains(OLDER), "{text}");
    // It fails again with another reason: that one is said.
    app.update(older_failed("reset twice"));
    let text = screen(&app);
    let lines = rows_of(&text);
    assert!(text.contains(TAGS_NOT_LOADED), "{text}");
    assert_eq!(
        lines[lines.len() - 3].trim(),
        format!("{OLDER}: reset twice"),
        "{text}"
    );
    // The list is whole: no row says anything about older runs.
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    let text = screen(&app);
    assert!(text.contains("all 250 runs loaded at"), "{text}");
    assert!(!text.contains(OLDER), "{text}");
}

#[test]
fn b974_the_tag_explorers_reason_wraps_on_a_narrow_terminal() {
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    app.update(older_failed(
        "the service did not answer the request for the next page in time",
    ));
    ch(&mut app, 't');
    // 60 columns do not hold the sentence: it wraps above the note line, and every word of it is on the screen.
    let text = screen_text(&app, 60, 30);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "could not load the older runs: the service did not answer the request for the next page in time"
        ),
        "{text}"
    );
    let (_, help) = bottom_two(&text);
    assert!(help.contains("Space pick"), "{text}");
}

#[test]
fn b974_the_tag_explorers_reason_takes_four_rows_at_most() {
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    app.update(older_failed(
        "the service did not answer the request for the next page in time, and the second try, the third try and the fourth try \
         ended as the first one did, so the read of the older runs was given up",
    ));
    ch(&mut app, 't');
    // 40 columns: the sentence needs more than four rows. It gets four, the fourth ends in an ellipsis, and the box above them and
    // the explorer's note line and key line under them are where they were.
    let text = screen_text(&app, 40, 30);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    assert!(
        lines[n - 7].starts_with('\u{2514}'),
        "the box ends above the sentence: {text}"
    );
    assert!(
        lines[n - 6].starts_with("could not load the older runs:"),
        "{text}"
    );
    assert!(
        lines[n - 3].ends_with('\u{2026}'),
        "a sentence that is cut ends in an ellipsis: {text}"
    );
    for row in &lines[n - 6..n - 2] {
        assert!(row.chars().count() <= 40, "{text}");
    }
    assert!(
        !lines[n - 2].contains("could not load") && !lines[n - 2].ends_with('\u{2026}'),
        "the note line is the explorer's own: {text}"
    );
    assert!(lines[n - 1].starts_with("Space pick"), "{text}");
}
