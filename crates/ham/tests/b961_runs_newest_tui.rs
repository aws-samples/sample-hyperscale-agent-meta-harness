//! The Runs screen while a whole read of the list is out: the read's newest page is laid as soon as it arrives (`Msg::RunsNewest`) and
//! the whole list when the read has ended (`Msg::RunsOlder`). Meanwhile the table's top border says that older runs are loading, the
//! counts are those of the runs on screen, and `R` and the tag explorer's `d` wait for the whole list. A read of the older runs that
//! fails leaves the newest page and says so. These tests drive `App::update` and draw on a test terminal; no time is asserted.
#![allow(clippy::unwrap_used)]

mod tagsfx;

use crossterm::event::KeyCode;
use ham::tui::{App, DirectSettings, Effect, MouseState, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, Refresh, RunsRead};
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};
use tagsfx::*;

const LOADING: &str = "\u{250c} runs \u{b7} loading older runs \u{2500}";
const NOT_LOADED: &str = "\u{250c} runs \u{b7} older runs not loaded \u{b7} r loads all \u{2500}";
const PLAIN: &str = "\u{250c} runs \u{2500}";
const NO_RUNS: &str = "no runs yet: press n to start one";
const ASKS_TWO: &str = "Resume 2 runs: each starts a new billable run from the code it delivered. Enter confirms \u{b7} Esc cancels";
const STILL_LOADING: &str = "older runs are still loading: press R again in a moment";
const NOT_EVERY: &str = "not every run is loaded: press r, then R";
const HINT: &str = "mouse is on: hold Shift to select text with the terminal";
/// A run that started after the list's newest run, and one of the same second as the newest run.
const X: usize = 900_000;
const Y: usize = 900_001;
/// A run that only a newest page holds, as old as run 99.
const Z: usize = 700_000;

/// The creation time `back` seconds before 12:00:00 (a negative `back` is later).
fn created(back: i64) -> String {
    let t = 43_200 - back;
    format!(
        "2026-10-20T{:02}:{:02}:{:02}Z",
        t / 3600,
        (t / 60) % 60,
        t % 60
    )
}

/// Run `n` of the stage: created `back` seconds before noon, done, in `calc` when `n` is even and in `web-shop` when it is odd.
fn run_at(n: usize, back: i64, extra: Value) -> RunSummary {
    let name = if n.is_multiple_of(2) {
        "calc"
    } else {
        "web-shop"
    };
    let mut v = json!({"runId": id(n), "status": "complete", "phase": "delivering",
        "target": {"mode": "repo", "repo": format!("cc:{name}")}, "createdAt": created(back), "ownerPrincipalArn": ARN});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(v).unwrap()
}

/// Runs `from..to`, newest first: run `n` was created `n` seconds before noon.
fn rows(from: usize, to: usize) -> Vec<RunSummary> {
    (from..to).map(|n| run_at(n, n as i64, json!({}))).collect()
}

/// The same with run 3 paused and run 150 stalled: the two runs `R` resumes.
fn rows_r(from: usize, to: usize) -> Vec<RunSummary> {
    (from..to)
        .map(|n| match n {
            3 => run_at(
                n,
                n as i64,
                json!({"status": "paused", "resumeMode": "follow_up", "session": 3,
                "statusReason": "total_time: the run used the total time it was given"}),
            ),
            150 => run_at(
                n,
                n as i64,
                json!({"status": "stalled", "resumeMode": "follow_up", "session": 2,
                "statusReason": "session_lost: the session stopped answering before it finished"}),
            ),
            _ => run_at(n, n as i64, json!({})),
        })
        .collect()
}

fn waits() -> Value {
    json!({"status": "active", "needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-20T13:00:00Z"}})
}

/// `list` with `extra` laid over the run `n`.
fn with(mut list: Vec<RunSummary>, n: usize, extra: Value) -> Vec<RunSummary> {
    let at = list.iter().position(|r| r.run_id == id(n)).unwrap();
    let back = 43_200 - parse(&list[at].created_at);
    list[at] = run_at(n, back, extra);
    list
}

fn parse(created: &str) -> i64 {
    let hms: Vec<i64> = created[11..19]
        .split(':')
        .map(|p| p.parse().unwrap())
        .collect();
    hms[0] * 3600 + hms[1] * 60 + hms[2]
}

/// `top` in front of `list`.
fn on_top(top: Vec<RunSummary>, list: Vec<RunSummary>) -> Vec<RunSummary> {
    top.into_iter().chain(list).collect()
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

/// A whole read that is one answer (the read of before).
fn whole(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index: vec![],
    }))
}

fn app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(unix(NOW));
    app
}

fn text(app: &App) -> String {
    screen_text(app, 140, 30)
}

/// The screen tall enough for every row of 260 runs.
fn tall(app: &App) -> String {
    screen_text(app, 140, 280)
}

fn top(text: &str) -> &str {
    text.lines()
        .find(|l| l.starts_with('\u{250c}'))
        .unwrap_or("")
}

fn bottom(text: &str) -> &str {
    text.lines()
        .find(|l| l.starts_with('\u{2514}'))
        .unwrap_or("")
}

/// The run the selection mark stands on.
fn marked(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let at = l.find("\u{25b6} ")?;
        l[at + "\u{25b6} ".len()..]
            .split_whitespace()
            .next()
            .map(str::to_string)
    })
}

fn has_line(text: &str, start: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(start))
}

fn rings(effects: &[Effect]) -> usize {
    effects.iter().filter(|e| **e == Effect::Bell).count()
}

fn notified(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(a) => Some(a.run_id.clone()),
            _ => None,
        })
        .collect()
}

fn refreshes(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::RefreshRuns(Refresh::Timer)))
        .count()
}

// ---- the newest page and the whole list ------------------------------------------------------------------------------------------

#[test]
fn b961_the_newest_page_is_on_screen_while_older_runs_are_loaded() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    let text = text(&app);
    assert!(
        text.contains(&id(0)),
        "the newest run's row is drawn: {text}"
    );
    assert!(top(&text).starts_with(LOADING), "{text}");
    assert!(bottom(&text).contains(" run 1 of 100 "), "{text}");
    assert!(
        has_line(&text, "100 runs"),
        "the counts are those of the runs on screen: {text}"
    );
}

#[test]
fn b961_a_list_of_exactly_one_full_page_ends_whole() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    assert!(top(&text(&app)).starts_with(LOADING), "{}", text(&app));
    // The second page was empty: the whole list is the same 100 runs.
    app.update(older(rows(0, 100)));
    let text = text(&app);
    assert!(top(&text).starts_with(PLAIN), "{text}");
    assert!(bottom(&text).contains(" run 1 of 100 "), "{text}");
    let all = tall(&app);
    for n in 0..100 {
        assert_eq!(
            all.matches(&id(n)).count(),
            1,
            "run {n} is listed once: {all}"
        );
    }
    ch(&mut app, 't');
    assert!(
        self::text(&app).contains("all 100 runs loaded at 14:05"),
        "{}",
        self::text(&app)
    );
}

#[test]
fn b961_every_run_is_there_when_the_read_has_ended() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    app.update(older(rows(0, 250)));
    let text = text(&app);
    assert!(top(&text).starts_with(PLAIN), "{text}");
    assert!(bottom(&text).contains(" run 1 of 250 "), "{text}");
    key(&mut app, KeyCode::End);
    let text = self::text(&app);
    assert!(text.contains(&id(249)), "End shows the oldest run: {text}");
    assert!(bottom(&text).contains(" run 250 of 250 "), "{text}");
    ch(&mut app, 't');
    assert!(
        self::text(&app).contains("all 250 runs loaded at 14:05"),
        "{}",
        self::text(&app)
    );
}

#[test]
fn b961_the_order_is_the_order_of_a_whole_read_alone() {
    let mut two = app();
    two.update(newest(rows(0, 100)));
    two.update(older(rows(0, 250)));
    let mut one = app();
    one.update(whole(rows(0, 250)));
    assert_eq!(tall(&two), tall(&one));
    assert!(
        tall(&one).contains(&id(249)),
        "TRIGGER ABSENT: the screen is tall enough for every row"
    );
}

#[test]
fn b961_the_selection_stays_on_its_run_when_the_older_runs_arrive() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    for _ in 0..39 {
        key(&mut app, KeyCode::Down);
    }
    assert_eq!(marked(&text(&app)), Some(id(39)), "{}", text(&app));
    // The whole list: two runs that started meanwhile on top, 150 older ones below.
    let new = vec![run_at(X + 1, -2, json!({})), run_at(X, -1, json!({}))];
    app.update(older(on_top(new, rows(0, 250))));
    let text = text(&app);
    assert_eq!(marked(&text), Some(id(39)), "{text}");
    assert!(bottom(&text).contains(" run 42 of 252 "), "{text}");
}

#[test]
fn b961_a_filter_typed_before_the_older_runs_arrive_holds_for_them() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    ch(&mut app, '/');
    type_in(&mut app, "web-shop");
    key(&mut app, KeyCode::Enter);
    let text = text(&app);
    assert!(text.contains("50 of 100 runs match \"web-shop\""), "{text}");
    let on = marked(&text);
    assert_eq!(on, Some(id(1)), "{text}");
    app.update(older(rows(0, 250)));
    let text = self::text(&app);
    assert!(
        text.contains("125 of 250 runs match \"web-shop\""),
        "{text}"
    );
    assert_eq!(marked(&text), on, "the marked run is the same: {text}");
    key(&mut app, KeyCode::End);
    assert!(
        self::text(&app).contains(&id(249)),
        "a match of the older pages is listed: {}",
        self::text(&app)
    );
    // No match among the newest runs: the first match is marked when the whole list lands.
    let calc_only = |n: usize| run_at(n * 2, n as i64, json!({}));
    let mut app = self::app();
    app.update(newest((0..100).map(calc_only).collect()));
    ch(&mut app, '/');
    type_in(&mut app, "web-shop");
    key(&mut app, KeyCode::Enter);
    let text = self::text(&app);
    assert!(
        text.contains("no run matches \"web-shop\" (100 runs)"),
        "{text}"
    );
    let mut all: Vec<RunSummary> = (0..100).map(calc_only).collect();
    all.extend((100..150).map(|n| run_at(n * 2 + 1, n as i64, json!({}))));
    app.update(older(all));
    let text = self::text(&app);
    assert!(text.contains("50 of 150 runs match \"web-shop\""), "{text}");
    assert_eq!(marked(&text), Some(id(201)), "{text}");
}

#[test]
fn b961_after_r_and_on_the_way_back_the_newest_page_refreshes_a_whole_list_in_place() {
    let mut app = app();
    app.update(whole(with(rows(0, 250), 0, json!({"status": "active"}))));
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    // The newest page of that read: run 0 has ended meanwhile.
    app.update(newest(rows(0, 100)));
    let text = text(&app);
    assert!(
        bottom(&text).contains(" run 1 of 250 "),
        "the 250 rows stay: {text}"
    );
    assert!(
        top(&text).starts_with(PLAIN),
        "the list on screen is whole: {text}"
    );
    let row = text.lines().find(|l| l.contains(&id(0))).unwrap();
    assert!(row.contains("complete") && !row.contains("active"), "{row}");
    // The whole list then replaces it.
    app.update(older(rows(0, 240)));
    let text = self::text(&app);
    assert!(bottom(&text).contains(" run 1 of 240 "), "{text}");
    assert!(top(&text).starts_with(PLAIN), "{text}");
}

#[test]
fn b961_no_runs_yet_is_said_only_after_an_answer() {
    let mut app = app();
    let text = text(&app);
    assert!(
        !text.contains(NO_RUNS),
        "nothing is known before the service has answered: {text}"
    );
    assert!(top(&text).starts_with(PLAIN), "{text}");
    app.update(whole(vec![]));
    let text = self::text(&app);
    assert!(text.contains(NO_RUNS), "{text}");
    assert!(top(&text).starts_with(PLAIN), "{text}");
}

// ---- a read of the older runs that fails -------------------------------------------------------------------------------------------

#[test]
fn b961_a_failed_read_of_the_older_runs_keeps_the_newest_page_and_says_so() {
    let mut app = app();
    app.update(newest(rows(0, 100)));
    app.update(older_failed("timeout"));
    let text = text(&app);
    assert!(text.contains(&id(0)), "the newest page stays: {text}");
    assert!(bottom(&text).contains(" run 1 of 100 "), "{text}");
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        text.contains("could not load the older runs: timeout"),
        "{text}"
    );
    assert!(!text.contains("cannot list runs"), "{text}");
    assert!(!text.contains("nextToken"), "{text}");
    // Every other failed read speaks with its own words, also directly after the failed read of the older runs (no read that
    // succeeds lies between the two): the timer's read, the read of before and the one-page read.
    for (name, failed) in [
        ("Msg::RunsIndexed", Msg::RunsIndexed(Err("reset".into()))),
        ("Msg::Runs", Msg::Runs(Err("reset".into()))),
        ("Msg::RunsRead", Msg::RunsRead(Err("reset".into()))),
    ] {
        let mut app = self::app();
        app.update(newest(rows(0, 100)));
        app.update(older_failed("timeout"));
        assert!(
            self::text(&app).contains("could not load the older runs: timeout"),
            "TRIGGER ABSENT ({name})"
        );
        app.update(failed);
        let text = self::text(&app);
        assert!(text.contains("cannot list runs: reset"), "{name}: {text}");
        assert!(
            !text.contains("could not load the older runs"),
            "{name}: {text}"
        );
        assert!(top(&text).starts_with(NOT_LOADED), "{name}: {text}");
    }
    // A timer read that succeeds clears the error's words and leaves the border: the older runs are still missing.
    app.update(timer(rows(0, 100)));
    let text = self::text(&app);
    assert!(!text.contains("could not load the older runs"), "{text}");
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    // A timer read that fails afterwards speaks for itself.
    app.update(Msg::RunsIndexed(Err("reset".into())));
    let text = self::text(&app);
    assert!(text.contains("cannot list runs: reset"), "{text}");
    assert!(!text.contains("could not load the older runs"), "{text}");
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    // `r` reads again; a read that ends whole restores the border and the counts.
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(newest(rows(0, 100)));
    assert!(
        top(&self::text(&app)).starts_with(LOADING),
        "{}",
        self::text(&app)
    );
    app.update(older(rows(0, 250)));
    let text = self::text(&app);
    assert!(top(&text).starts_with(PLAIN), "{text}");
    assert!(bottom(&text).contains(" run 1 of 250 "), "{text}");
    assert!(has_line(&text, "250 runs"), "{text}");
    assert!(
        !text.contains("cannot list runs") && !text.contains("could not load"),
        "{text}"
    );

    // On a first screen with the mouse hint the note shows the hint; the border still carries the state.
    let mut app = self::app();
    app.set_mouse(
        MouseState {
            on: true,
            forced_off: false,
        },
        Some(HINT.into()),
    );
    app.update(newest(rows(0, 100)));
    app.update(older_failed("timeout"));
    let text = self::text(&app);
    assert!(text.contains(HINT), "{text}");
    assert!(!text.contains("could not load the older runs"), "{text}");
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");

    // A list that was whole before keeps its rows, its border and the words of before.
    let mut app = self::app();
    app.update(whole(rows(0, 250)));
    app.update(newest(rows(0, 100)));
    app.update(older_failed("timeout"));
    let text = self::text(&app);
    assert!(bottom(&text).contains(" run 1 of 250 "), "{text}");
    assert!(top(&text).starts_with(PLAIN), "{text}");
    assert!(text.contains("cannot list runs: timeout"), "{text}");
    assert!(!text.contains("could not load the older runs"), "{text}");

    // A first page that fails with no row held says the words of before.
    let mut app = self::app();
    app.update(Msg::RunsIndexed(Err("down".into())));
    let text = self::text(&app);
    assert!(text.contains("cannot list runs: down"), "{text}");
    assert!(top(&text).starts_with(PLAIN), "{text}");
}

// ---- the bell ----------------------------------------------------------------------------------------------------------------------

fn listening() -> App {
    let mut app = app();
    app.set_notify(true);
    app.set_focus(false);
    app
}

#[test]
fn b961_an_older_run_with_an_open_question_rings_no_bell_when_the_list_is_completed() {
    // The older pages hold a run that has waited since before the session began.
    let mut app = listening();
    let first = app.update(newest(rows(0, 100)));
    assert_eq!(rings(&first), 0, "{first:?}");
    let done = app.update(older(with(rows(0, 250), 150, waits())));
    assert_eq!(
        rings(&done),
        0,
        "an old run with an open question makes no sound: {done:?}"
    );
    assert!(notified(&done).is_empty(), "{done:?}");
    assert!(
        has_line(&text(&app), "250 runs \u{b7} 1 needs input"),
        "TRIGGER ABSENT: {}",
        text(&app)
    );

    // A run that started while the older pages were read, with a question: it rings once. The old one stays silent.
    let mut app = listening();
    app.update(newest(rows(0, 100)));
    let list = on_top(
        vec![run_at(X, -5, waits())],
        with(rows(0, 250), 150, waits()),
    );
    let done = app.update(older(list));
    assert_eq!(rings(&done), 1, "{done:?}");
    assert_eq!(notified(&done), vec![id(X)], "{done:?}");
    // Afterwards a held run that begins to wait rings once, as before.
    let begun = with(
        on_top(vec![run_at(X, -5, waits())], rows(0, 99)),
        5,
        waits(),
    );
    let later = app.update(timer(begun));
    assert_eq!(rings(&later), 1, "{later:?}");
    assert_eq!(notified(&later), vec![id(5)], "{later:?}");
}

#[test]
fn b961_a_whole_read_after_a_one_page_read_rings_no_bell_for_old_runs() {
    // A chat opened without the Runs screen reads one newest page; the Runs screen then reads every page.
    let mut app = listening();
    app.update(timer(rows(0, 100)));
    let first = app.update(newest(rows(0, 100)));
    assert_eq!(rings(&first), 0, "{first:?}");
    let done = app.update(older(with(rows(0, 250), 150, waits())));
    assert_eq!(rings(&done), 0, "{done:?}");
    assert!(notified(&done).is_empty(), "{done:?}");
    assert!(
        has_line(&text(&app), "250 runs \u{b7} 1 needs input"),
        "TRIGGER ABSENT: {}",
        text(&app)
    );
}

// ---- the tag explorer and R wait for the whole list ---------------------------------------------------------------------------------

fn tagged(from: usize, to: usize) -> Vec<IndexEntry> {
    (from..to)
        .map(|n| entry(&id(n), Some(&["topic:old"])))
        .collect()
}

#[test]
fn b961_the_tag_explorer_says_that_older_runs_are_loading() {
    const ASKS: &str = "Remove topic:old from 250 runs? y/n";
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    ch(&mut app, 't');
    let text = screen(&app);
    assert!(
        text.contains("\u{250c} tags \u{b7} newest 100 runs, loading the older ones "),
        "{text}"
    );
    assert!(ch(&mut app, 'd').is_empty());
    let text = screen(&app);
    assert_eq!(
        bottom_two(&text).0,
        "remove needs every run loaded: they are still loading",
        "{text}"
    );
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    let text = screen(&app);
    assert!(text.contains("all 250 runs loaded at 14:05"), "{text}");
    ch(&mut app, 'd');
    assert_eq!(bottom_two(&screen(&app)).0, ASKS);
    key(&mut app, KeyCode::Esc);
    // With the whole list held `d` asks as before while the read of `r` is out, and after that read failed.
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    assert!(
        screen(&app).contains("all 250 runs loaded at 14:05"),
        "{}",
        screen(&app)
    );
    ch(&mut app, 'd');
    assert_eq!(bottom_two(&screen(&app)).0, ASKS);
    key(&mut app, KeyCode::Esc);
    app.update(older_failed("timeout"));
    ch(&mut app, 'd');
    assert_eq!(bottom_two(&screen(&app)).0, ASKS);
}

#[test]
#[allow(non_snake_case)]
fn b961_R_waits_for_the_whole_list() {
    let mut app = app();
    app.update(newest(rows_r(0, 100)));
    // Run 3 is paused and on screen, run 150 is stalled and not loaded yet: R plans nothing from a part.
    assert!(ch(&mut app, 'R').is_empty());
    let text = text(&app);
    assert!(text.contains(STILL_LOADING), "{text}");
    assert!(!text.contains("Enter confirms"), "no question: {text}");
    app.update(older(rows_r(0, 250)));
    key(&mut app, KeyCode::Home);
    ch(&mut app, 'R');
    assert!(self::text(&app).contains(ASKS_TWO), "{}", self::text(&app));
    key(&mut app, KeyCode::Esc);

    // After a failed read of the older runs R names the key that loads them.
    let mut app = self::app();
    app.update(newest(rows_r(0, 100)));
    app.update(older_failed("timeout"));
    assert!(ch(&mut app, 'R').is_empty());
    let text = self::text(&app);
    assert!(text.contains(NOT_EVERY), "{text}");
    assert!(!text.contains("Enter confirms"), "{text}");

    // With the whole list held R asks as before while the read of `r` is out, and after that read failed.
    let mut app = self::app();
    app.update(whole(rows_r(0, 250)));
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(newest(rows_r(0, 100)));
    ch(&mut app, 'R');
    assert!(self::text(&app).contains(ASKS_TWO), "{}", self::text(&app));
    key(&mut app, KeyCode::Esc);
    app.update(older_failed("timeout"));
    ch(&mut app, 'R');
    assert!(self::text(&app).contains(ASKS_TWO), "{}", self::text(&app));
}

// ---- a run that started while the list was being read ------------------------------------------------------------------------------

/// The newest page a timer read shows once X (with a question) and Y (of the newest run's second) exist: both on top of runs 0 to 97.
fn page_with_x_and_y() -> IndexedRead {
    let top = vec![run_at(X, -5, waits()), run_at(Y, 0, json!({}))];
    let title = IndexEntry {
        title: Some("X-TITLE".into()),
        ..entry(&id(X), None)
    };
    page(on_top(top, rows(0, 98)), vec![title])
}

#[test]
fn b961_a_run_that_started_while_the_older_runs_were_loaded_stays_and_stays_selected() {
    let mut app = listening();
    // The newest page: runs 0 to 98 and Z, a run as old as run 99 that only this page holds.
    let mut first = rows(0, 99);
    first.push(run_at(Z, 99, json!({})));
    app.update(newest(first));
    // A timer page shows X and Y; X asks a question.
    let shown = app.update(Msg::RunsIndexed(Ok(page_with_x_and_y())));
    assert_eq!(
        rings(&shown),
        1,
        "the bell for X rings at the timer page: {shown:?}"
    );
    key(&mut app, KeyCode::Home);
    assert_eq!(marked(&text(&app)), Some(id(X)), "{}", text(&app));
    assert!(
        text(&app).contains("X-TITLE"),
        "TRIGGER ABSENT: {}",
        text(&app)
    );
    assert!(tall(&app).contains(&id(Z)), "TRIGGER ABSENT: Z is held");
    // The whole list: its first page was read before X and Y existed.
    let done = app.update(older(rows(0, 250)));
    assert_eq!(rings(&done), 0, "{done:?}");
    assert!(notified(&done).is_empty(), "{done:?}");
    let text = text(&app);
    assert_eq!(marked(&text), Some(id(X)), "X is still marked: {text}");
    assert!(
        bottom(&text).contains(" run 1 of 252 "),
        "X and Y stay on top of the 250: {text}"
    );
    assert!(
        text.contains("X-TITLE"),
        "what the index holds about X is kept: {text}"
    );
    let all = tall(&app);
    let (x, y, zero) = (
        all.find(&id(X)).unwrap(),
        all.find(&id(Y)).unwrap(),
        all.find(&id(0)).unwrap(),
    );
    assert!(
        x < y && y < zero,
        "X, then Y of the newest run's second, then the whole list"
    );
    assert_eq!(all.matches(&id(X)).count(), 1);
    assert!(
        !all.contains(&id(Z)),
        "a held run that is older than the list's newest run and not in the list is gone"
    );
    assert!(top(&text).starts_with(PLAIN), "{text}");
    // The next timer page: X still waits and does not ring again.
    let next = app.update(Msg::RunsIndexed(Ok(page_with_x_and_y())));
    assert_eq!(rings(&next), 0, "{next:?}");
}

#[test]
fn b961_the_two_new_messages_leave_the_timers_flag_alone() {
    let mut app = app();
    app.update(timer(rows(0, 100)));
    let mut asked = 0;
    for _ in 0..5 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 1, "TRIGGER ABSENT: a timer read is out");
    // A whole read's two messages arrive while the timer's read is still out.
    app.update(newest(rows(0, 100)));
    app.update(older(rows(0, 250)));
    for _ in 0..20 {
        assert_eq!(
            refreshes(&app.tick()),
            0,
            "no second timer read while the first is out"
        );
    }
    // The timer's answer comes; the next interval asks again.
    app.update(timer(rows(0, 100)));
    let mut asked = 0;
    for _ in 0..5 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 1);
}

#[test]
fn b961_a_newest_page_keeps_a_run_that_a_timer_page_showed() {
    let mut app = app();
    app.update(timer(rows(0, 100)));
    // A timer page shows the new run X, and the person moves onto it.
    app.update(timer(on_top(vec![run_at(X, -5, json!({}))], rows(0, 99))));
    key(&mut app, KeyCode::Home);
    assert_eq!(
        marked(&text(&app)),
        Some(id(X)),
        "TRIGGER ABSENT: {}",
        text(&app)
    );
    // The newest page of a whole read whose call was the slower one: it was read before X existed.
    app.update(newest(rows(0, 100)));
    let text = text(&app);
    assert_eq!(marked(&text), Some(id(X)), "{text}");
    assert!(bottom(&text).contains(" run 1 of 101 "), "{text}");
}

// ---- two whole reads whose messages interleave -------------------------------------------------------------------------------------

#[test]
fn b961_two_whole_reads_whose_messages_interleave_end_right() {
    // Read A was sent before read B. X started between their first pages, with a question: B's pages hold it, A's do not.
    let x = || vec![run_at(X, -5, waits())];
    let na = || newest(rows_r(0, 100));
    let nb = || newest(on_top(x(), rows_r(0, 99)));
    let oa = || older(rows_r(0, 250));
    let ob = || older(on_top(x(), rows_r(0, 250)));
    // Each message of an order: the bells it rang (this test holds the bell of X only), then the screen.
    let play = |order: Vec<Msg>| -> (App, Vec<usize>, Vec<String>) {
        let mut app = listening();
        let mut bells = Vec::new();
        let mut screens = Vec::new();
        for m in order {
            bells.push(rings(&app.update(m)));
            screens.push(text(&app));
        }
        (app, bells, screens)
    };
    let whole_with_x = |app: &mut App, what: &str| {
        let text = text(app);
        assert!(top(&text).starts_with(PLAIN), "{what}: {text}");
        assert!(bottom(&text).contains(" of 251 "), "{what}: {text}");
        let all = tall(app);
        assert_eq!(all.matches(&id(X)).count(), 1, "{what}: X is listed once");
        assert!(
            all.find(&id(X)).unwrap() < all.find(&id(0)).unwrap(),
            "{what}: X is on top"
        );
        // The list is whole: R plans from it.
        ch(app, 'R');
        assert!(
            self::text(app).contains(ASKS_TWO),
            "{what}: {}",
            self::text(app)
        );
        key(app, KeyCode::Esc);
    };

    // Order 1: NA, NB, OA, OB.
    let (mut app, bells, screens) = play(vec![na(), nb(), oa(), ob()]);
    assert_eq!(bells, [0, 1, 0, 0], "one bell for X, at NB");
    assert!(
        top(&screens[0]).starts_with(LOADING) && bottom(&screens[0]).contains(" of 100 "),
        "{}",
        screens[0]
    );
    assert!(
        top(&screens[1]).starts_with(LOADING) && bottom(&screens[1]).contains(" of 101 "),
        "{}",
        screens[1]
    );
    assert!(screens[1].contains(&id(X)), "{}", screens[1]);
    assert!(
        top(&screens[2]).starts_with(PLAIN) && bottom(&screens[2]).contains(" of 251 "),
        "A's list with X kept on top: {}",
        screens[2]
    );
    assert!(screens[2].contains(&id(X)), "{}", screens[2]);
    whole_with_x(&mut app, "order 1");

    // Order 2: NA, NB, OB, OA.
    let (mut app, bells, screens) = play(vec![na(), nb(), ob(), oa()]);
    assert_eq!(bells, [0, 1, 0, 0]);
    assert!(
        top(&screens[2]).starts_with(PLAIN) && bottom(&screens[2]).contains(" of 251 "),
        "{}",
        screens[2]
    );
    whole_with_x(&mut app, "order 2");

    // Order 3: NA, NB, OB, then A's older runs fail: the list stays B's and whole, and the words are those of before.
    let (mut app, bells, screens) = play(vec![na(), nb(), ob(), older_failed("A failed")]);
    assert_eq!(bells, [0, 1, 0, 0]);
    assert!(
        screens[3].contains("cannot list runs: A failed"),
        "{}",
        screens[3]
    );
    assert!(
        !screens[3].contains("could not load the older runs"),
        "{}",
        screens[3]
    );
    whole_with_x(&mut app, "order 3");

    // A fourth order, its end: A's older runs fail while B still loads; B's whole list ends it.
    let (mut app, bells, screens) = play(vec![na(), nb(), older_failed("A failed"), ob()]);
    assert_eq!(bells, [0, 1, 0, 0]);
    assert!(
        !screens[3].contains("cannot list runs") && !screens[3].contains("could not load"),
        "{}",
        screens[3]
    );
    whole_with_x(&mut app, "the fourth order");
}

// ---- a sentence about a list that is not whole never stays once that is no longer so ------------------------------------------------

const D_STILL_LOADING: &str = "remove needs every run loaded: they are still loading";
const D_NOT_LOADED: &str = "remove needs every run loaded: press r to load them all, then d";

/// Every message that leaves the list whole, as the app can get it, with a name for the failure text.
fn ends_whole() -> Vec<(&'static str, Msg)> {
    let one_page = || RunsRead::First {
        runs: rows_r(0, 100),
        has_more: false,
    };
    vec![
        ("the end of a whole read", older(rows_r(0, 250))),
        ("every page in one answer", whole(rows_r(0, 250))),
        (
            "a newest page with no next token",
            Msg::RunsIndexed(Ok(IndexedRead {
                read: one_page(),
                index: vec![],
            })),
        ),
        (
            "every page, Msg::RunsRead",
            Msg::RunsRead(Ok(RunsRead::All(rows_r(0, 250)))),
        ),
        (
            "a newest page with no next token, Msg::RunsRead",
            Msg::RunsRead(Ok(one_page())),
        ),
        ("every page, Msg::Runs", Msg::Runs(Ok(rows_r(0, 250)))),
    ]
}

#[test]
#[allow(non_snake_case)]
fn b961_Rs_refusal_does_not_stay_once_the_list_is_whole() {
    let count = ends_whole().len();
    assert_eq!(count, 6);
    for i in 0..count {
        // Older runs are being loaded and R was refused. The list becomes whole; no key is pressed.
        let (name, end) = ends_whole().remove(i);
        let mut app = app();
        app.update(newest(rows_r(0, 100)));
        assert!(ch(&mut app, 'R').is_empty());
        assert!(
            text(&app).contains(STILL_LOADING),
            "TRIGGER ABSENT ({name})"
        );
        app.update(end);
        let text = text(&app);
        assert!(top(&text).starts_with(PLAIN), "{name}: {text}");
        assert!(
            !text.contains(STILL_LOADING),
            "{name}: the list is whole and the note still says that older runs are loading: {text}"
        );

        // The older runs failed and R named the key that loads them. A read leaves the list whole; no key is pressed.
        let (name, end) = ends_whole().remove(i);
        let mut app = self::app();
        app.update(newest(rows_r(0, 100)));
        app.update(older_failed("timeout"));
        assert!(ch(&mut app, 'R').is_empty());
        assert!(
            self::text(&app).contains(NOT_EVERY),
            "TRIGGER ABSENT ({name})"
        );
        app.update(end);
        let text = self::text(&app);
        assert!(top(&text).starts_with(PLAIN), "{name}: {text}");
        assert!(
            !text.contains(NOT_EVERY),
            "{name}: the list is whole and the note still says that not every run is loaded: {text}"
        );
    }
}

#[test]
fn b961_the_tag_explorers_note_does_not_stay_once_the_list_is_whole() {
    // Older runs are being loaded and `d` was refused. The whole list lands; no key is pressed.
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    ch(&mut app, 't');
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(
        bottom_two(&screen(&app)).0,
        D_STILL_LOADING,
        "TRIGGER ABSENT"
    );
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    let text = screen(&app);
    assert!(text.contains("all 250 runs loaded at 14:05"), "{text}");
    assert!(
        !text.contains("they are still loading"),
        "the list is whole and the note still says that the runs are loading: {text}"
    );

    // The older runs failed and `d` named the key that loads them. Every page arrives in one answer; no key is pressed.
    let mut app = self::app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    app.update(older_failed("timeout"));
    ch(&mut app, 't');
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(bottom_two(&screen(&app)).0, D_NOT_LOADED, "TRIGGER ABSENT");
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    let text = screen(&app);
    assert!(text.contains("all 250 runs loaded at 14:05"), "{text}");
    assert!(
        !text.contains("press r to load them all"),
        "the list is whole and the note still names the key that loads it: {text}"
    );
}

#[test]
#[allow(non_snake_case)]
fn b961_a_failed_read_of_the_older_runs_takes_the_still_loading_notes_away() {
    // R was refused while older runs were being loaded; then their read fails. No key is pressed. The screen says what the
    // failed read says on any other screen: the border names the key, the note line the error.
    let mut app = app();
    app.update(newest(rows_r(0, 100)));
    assert!(ch(&mut app, 'R').is_empty());
    assert!(text(&app).contains(STILL_LOADING), "TRIGGER ABSENT");
    app.update(older_failed("timeout"));
    let text = text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        !text.contains(STILL_LOADING),
        "the older runs failed and the note still says that they are loading: {text}"
    );
    assert!(
        text.contains("could not load the older runs: timeout"),
        "{text}"
    );
    assert!(bottom(&text).contains(" run 1 of 100 "), "{text}");

    // The tag explorer the same way: its title names the key, and its note no longer says that the runs are loading.
    let mut app = self::app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    ch(&mut app, 't');
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(
        bottom_two(&screen(&app)).0,
        D_STILL_LOADING,
        "TRIGGER ABSENT"
    );
    app.update(older_failed("timeout"));
    let text = screen(&app);
    assert!(
        text.contains("\u{250c} tags \u{b7} newest 100 runs only - r loads all "),
        "{text}"
    );
    assert!(
        !text.contains("still loading"),
        "the older runs failed and the note still says that they are loading: {text}"
    );
}

#[test]
#[allow(non_snake_case)]
fn b961_a_note_of_another_kind_stays_when_the_list_becomes_whole() {
    const NOTHING: &str = "no paused or stalled run to resume";
    // R's question (a whole list is held and the read of `r` is out): the end of that read leaves it, and so does its failure.
    for end in [older(rows_r(0, 250)), older_failed("timeout")] {
        let mut app = app();
        app.update(whole(rows_r(0, 250)));
        assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
        app.update(newest(rows_r(0, 100)));
        ch(&mut app, 'R');
        assert!(text(&app).contains(ASKS_TWO), "TRIGGER ABSENT");
        app.update(end);
        assert!(text(&app).contains(ASKS_TWO), "{}", text(&app));
    }
    // R's answer that no run waits, and every message that leaves the list whole.
    let count = ends_whole().len();
    for i in 0..count {
        let (name, end) = ends_whole().remove(i);
        let mut app = app();
        app.update(whole(rows(0, 250)));
        ch(&mut app, 'R');
        assert!(text(&app).contains(NOTHING), "TRIGGER ABSENT ({name})");
        app.update(end);
        assert!(text(&app).contains(NOTHING), "{name}: {}", text(&app));
    }
    // The same answer while older runs are loaded after it, and when their read fails (a list that was whole and has a gap now
    // would be another case: here the list stays whole).
    let mut app = app();
    app.update(whole(rows(0, 250)));
    ch(&mut app, 'R');
    app.update(newest(rows(0, 100)));
    assert!(text(&app).contains(NOTHING), "{}", text(&app));
    app.update(older_failed("timeout"));
    assert!(text(&app).contains(NOTHING), "{}", text(&app));
}

/// The tag explorer's half of the same rule: the note of a removal that is running stays when a message of the list arrives (each
/// message that leaves the list whole, and the timer's page).
#[test]
fn b961_a_tag_explorer_note_of_another_kind_stays_when_a_list_message_arrives() {
    const REMOVING: &str = "removing topic:old from 250 runs...";
    let count = ends_whole().len();
    for i in 0..=count {
        let mut app = app();
        app.update(Msg::RunsIndexed(Ok(IndexedRead {
            read: RunsRead::All(rows(0, 250)),
            index: tagged(0, 250),
        })));
        ch(&mut app, 't');
        ch(&mut app, 'd');
        let sent = ch(&mut app, 'y');
        assert_eq!(sent.len(), 1, "TRIGGER ABSENT: y sent no removal");
        assert_eq!(
            bottom_two(&screen(&app)).0,
            REMOVING,
            "TRIGGER ABSENT: the note of the removal is not on the screen"
        );
        let (name, msg) = if i < count {
            ends_whole().remove(i)
        } else {
            ("the timer's page", timer(rows(0, 100)))
        };
        app.update(msg);
        assert_eq!(
            bottom_two(&screen(&app)).0,
            REMOVING,
            "{name}: the note of the removal was cleared by a message of the list"
        );
    }
}

#[test]
fn b961_a_read_that_is_itself_the_whole_list_ends_the_state_of_the_older_runs() {
    // Older runs are being loaded. A read arrives that is itself the whole list (every page in one answer, or a newest page with no
    // next token), through each of the two messages that carry such a read: the state of the older runs ends with it. A later
    // page that leaves a gap (a next token, and no run shared with the held list) makes the list a part again, and no read of
    // older runs is out: the border must not say that they are loading, and R names the key.
    for i in 1..=4 {
        let (name, read) = ends_whole().remove(i);
        let mut app = app();
        app.update(newest(rows_r(0, 100)));
        assert!(
            top(&text(&app)).starts_with(LOADING),
            "TRIGGER ABSENT ({name})"
        );
        app.update(read);
        assert!(top(&text(&app)).starts_with(PLAIN), "{name}");
        app.update(timer(rows(300, 400)));
        let text = text(&app);
        assert!(
            !top(&text).starts_with(LOADING),
            "{name}: no read of older runs is out: {text}"
        );
        assert!(ch(&mut app, 'R').is_empty());
        let text = self::text(&app);
        assert!(text.contains(NOT_EVERY), "{name}: {text}");
        assert!(!text.contains(STILL_LOADING), "{name}: {text}");
    }
}

// ----: a sentence that names `r` becomes the still-loading one when older runs begin to load ----------------------------------

const TAGS_NOT_LOADED: &str = "\u{250c} tags \u{b7} newest 100 runs only - r loads all ";
const TAGS_LOADING: &str = "\u{250c} tags \u{b7} newest 100 runs, loading the older ones ";

/// The Runs screen after a failed read of the older runs, then `r` and `R` before the newest page of the new read has answered: the
/// border says that the older runs are not loaded, the note line names `r`, and a whole read is out.
#[allow(non_snake_case)]
fn R_names_r_while_a_read_is_out() -> App {
    let mut app = app();
    app.update(newest(rows_r(0, 100)));
    app.update(older_failed("timeout"));
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    assert!(ch(&mut app, 'R').is_empty());
    let text = text(&app);
    assert!(
        top(&text).starts_with(NOT_LOADED) && text.contains(NOT_EVERY),
        "TRIGGER ABSENT: {text}"
    );
    app
}

/// The tag explorer in the same state: `t`, `r` and `d` before the newest page of the new read has answered.
fn d_names_r_while_a_read_is_out() -> App {
    let mut app = app();
    app.update(Msg::RunsNewest(page(rows(0, 100), tagged(0, 100))));
    app.update(older_failed("timeout"));
    ch(&mut app, 't');
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    assert!(ch(&mut app, 'd').is_empty());
    let text = screen(&app);
    assert!(text.contains(TAGS_NOT_LOADED), "TRIGGER ABSENT: {text}");
    assert_eq!(bottom_two(&text).0, D_NOT_LOADED, "TRIGGER ABSENT");
    app
}

/// The newest page of the read that `r` started, for the tag explorer's rows.
fn newest_tagged() -> Msg {
    Msg::RunsNewest(page(rows(0, 100), tagged(0, 100)))
}

#[test]
#[allow(non_snake_case)]
fn b971_Rs_sentence_that_names_r_becomes_the_still_loading_one_when_older_runs_begin_to_load() {
    let mut app = R_names_r_while_a_read_is_out();
    // The newest page of the read that `r` started lands: older runs are being loaded. No key is pressed.
    app.update(newest(rows_r(0, 100)));
    let text = text(&app);
    assert!(top(&text).starts_with(LOADING), "TRIGGER ABSENT: {text}");
    assert!(
        !text.contains(NOT_EVERY),
        "older runs are loading and the note still says to press r: {text}"
    );
    assert!(text.contains(STILL_LOADING), "{text}");
    assert!(!text.contains("Enter confirms"), "no question: {text}");
}

#[test]
fn b971_the_tag_explorers_sentence_that_names_r_becomes_the_still_loading_one_when_older_runs_begin_to_load()
 {
    let mut app = d_names_r_while_a_read_is_out();
    app.update(newest_tagged());
    let text = screen(&app);
    assert!(text.contains(TAGS_LOADING), "TRIGGER ABSENT: {text}");
    assert_eq!(
        bottom_two(&text).0,
        D_STILL_LOADING,
        "older runs are loading and the note still says to press r: {text}"
    );
}

#[test]
fn b971_both_sentences_go_when_the_list_is_whole() {
    // The Runs screen, through every message that leaves the list whole. No key is pressed after `R`.
    let count = ends_whole().len();
    for i in 0..count {
        let (name, end) = ends_whole().remove(i);
        let mut app = R_names_r_while_a_read_is_out();
        app.update(newest(rows_r(0, 100)));
        assert!(
            text(&app).contains(STILL_LOADING),
            "{name}: while older runs load the note is the still-loading sentence: {}",
            text(&app)
        );
        app.update(end);
        let text = text(&app);
        assert!(top(&text).starts_with(PLAIN), "{name}: {text}");
        assert!(
            !text.contains(STILL_LOADING) && !text.contains(NOT_EVERY),
            "{name}: the list is whole and a sentence about a part of it still stands: {text}"
        );
    }
    // The tag explorer, through the end of the whole read.
    let mut app = d_names_r_while_a_read_is_out();
    app.update(newest_tagged());
    assert_eq!(
        bottom_two(&screen(&app)).0,
        D_STILL_LOADING,
        "while older runs load the note is the still-loading sentence"
    );
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    let text = screen(&app);
    assert!(text.contains("all 250 runs loaded at 14:05"), "{text}");
    assert!(
        !text.contains("remove needs every run loaded"),
        "the list is whole and a sentence about a part of it still stands: {text}"
    );
}

#[test]
fn b971_both_sentences_name_r_again_when_the_older_runs_fail_again() {
    // The Runs screen: the sentence named `r`, became the still-loading one, and the read of the older runs fails again.
    let mut app = R_names_r_while_a_read_is_out();
    app.update(newest(rows_r(0, 100)));
    assert!(
        text(&app).contains(STILL_LOADING),
        "while older runs load the note is the still-loading sentence: {}",
        text(&app)
    );
    app.update(older_failed("reset"));
    let text = text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        text.contains(NOT_EVERY),
        "the older runs failed again and the note does not name r: {text}"
    );
    assert!(!text.contains(STILL_LOADING), "{text}");
    // A third read: the sentence follows the older runs again, and goes when the list is whole.
    app.update(newest(rows_r(0, 100)));
    assert!(self::text(&app).contains(STILL_LOADING));
    assert!(!self::text(&app).contains(NOT_EVERY));
    app.update(older_failed("reset twice"));
    assert!(self::text(&app).contains(NOT_EVERY));
    // The failure's words are there as before: the next key takes the note away and the note line shows them.
    key(&mut app, KeyCode::Down);
    let text = self::text(&app);
    assert!(
        text.contains("could not load the older runs: reset twice"),
        "{text}"
    );
    assert!(!text.contains(NOT_EVERY), "{text}");

    // The tag explorer the same way.
    let mut app = d_names_r_while_a_read_is_out();
    app.update(newest_tagged());
    assert_eq!(
        bottom_two(&screen(&app)).0,
        D_STILL_LOADING,
        "while older runs load the note is the still-loading sentence"
    );
    app.update(older_failed("reset"));
    let text = screen(&app);
    assert!(text.contains(TAGS_NOT_LOADED), "{text}");
    assert_eq!(
        bottom_two(&text).0,
        D_NOT_LOADED,
        "the older runs failed again and the note does not name r: {text}"
    );
    app.update(newest_tagged());
    assert_eq!(bottom_two(&screen(&app)).0, D_STILL_LOADING);
    app.update(older_failed("reset twice"));
    assert_eq!(bottom_two(&screen(&app)).0, D_NOT_LOADED);
}

#[test]
#[allow(non_snake_case)]
fn b971_R_and_d_answer_by_the_state_of_the_list_as_before() {
    const ASKS_D: &str = "Remove topic:old from 250 runs? y/n";
    // The Runs screen. Over a failed part `R` names `r` (the helper asserts it). While older runs load `R` says so, also when the
    // note line held the sentence that had named `r`.
    let mut app = R_names_r_while_a_read_is_out();
    app.update(newest(rows_r(0, 100)));
    assert!(ch(&mut app, 'R').is_empty());
    let text = text(&app);
    assert!(text.contains(STILL_LOADING), "{text}");
    assert!(!text.contains(NOT_EVERY), "{text}");
    assert!(!text.contains("Enter confirms"), "no question: {text}");
    // That sentence was set by the key while older runs were loading: when their read fails it goes and the failed read speaks.
    app.update(older_failed("reset"));
    let text = self::text(&app);
    assert!(top(&text).starts_with(NOT_LOADED), "{text}");
    assert!(
        text.contains("could not load the older runs: reset"),
        "{text}"
    );
    assert!(
        !text.contains(STILL_LOADING) && !text.contains(NOT_EVERY),
        "{text}"
    );
    // Over the failed part `R` names `r` again; with the whole list held it asks.
    assert!(ch(&mut app, 'R').is_empty());
    assert!(self::text(&app).contains(NOT_EVERY));
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(newest(rows_r(0, 100)));
    app.update(older(rows_r(0, 250)));
    key(&mut app, KeyCode::Home);
    ch(&mut app, 'R');
    assert!(self::text(&app).contains(ASKS_TWO), "{}", self::text(&app));

    // The tag explorer: `d` by the same states.
    let mut app = d_names_r_while_a_read_is_out();
    app.update(newest_tagged());
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(bottom_two(&screen(&app)).0, D_STILL_LOADING);
    app.update(older_failed("reset"));
    let text = screen(&app);
    assert!(text.contains(TAGS_NOT_LOADED), "{text}");
    assert!(
        !text.contains("remove needs every run loaded"),
        "the key set the sentence while older runs were loading, and their read failed: {text}"
    );
    assert!(ch(&mut app, 'd').is_empty());
    assert_eq!(bottom_two(&screen(&app)).0, D_NOT_LOADED);
    assert_eq!(ch(&mut app, 'r'), vec![Effect::LoadRuns]);
    app.update(newest_tagged());
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    ch(&mut app, 'd');
    assert_eq!(bottom_two(&screen(&app)).0, ASKS_D);
}

#[test]
fn b971_a_note_of_another_kind_is_not_changed_when_older_runs_begin_to_load() {
    const PASTE: &str = "paste ignored: this screen has no text box";
    const REMOVING: &str = "removing topic:old from 250 runs...";
    // The Runs screen: `R` named `r`, then a paste put its own note there. The messages of two reads of the older runs leave it.
    let mut app = R_names_r_while_a_read_is_out();
    assert!(app.handle_paste("x".to_string()).is_empty());
    assert!(text(&app).contains(PASTE), "TRIGGER ABSENT");
    for (name, msg) in [
        ("the newest page of a whole read", newest(rows_r(0, 100))),
        ("the failure of the older runs", older_failed("reset")),
        ("the newest page of the next read", newest(rows_r(0, 100))),
        ("the timer's page", timer(rows_r(0, 100))),
        ("the end of the read", older(rows_r(0, 250))),
    ] {
        app.update(msg);
        let text = text(&app);
        assert!(text.contains(PASTE), "{name}: {text}");
        assert!(
            !text.contains(STILL_LOADING) && !text.contains(NOT_EVERY),
            "{name}: {text}"
        );
    }

    // The tag explorer: `d` named `r`; then the whole list arrived and a removal was sent, whose note stands. A timer's page that
    // leaves a gap makes the list a part again, and the messages of two reads of the older runs leave the note.
    let mut app = d_names_r_while_a_read_is_out();
    app.update(newest_tagged());
    app.update(Msg::RunsOlder(Ok(IndexedRead {
        read: RunsRead::All(rows(0, 250)),
        index: tagged(0, 250),
    })));
    ch(&mut app, 'd');
    assert_eq!(
        ch(&mut app, 'y').len(),
        1,
        "TRIGGER ABSENT: y sent no removal"
    );
    assert_eq!(bottom_two(&screen(&app)).0, REMOVING, "TRIGGER ABSENT");
    app.update(timer(rows(300, 400)));
    assert!(
        screen(&app).contains("\u{250c} tags \u{b7} newest "),
        "TRIGGER ABSENT: the list is a part again: {}",
        screen(&app)
    );
    for (name, msg) in [
        ("the timer's page that leaves a gap", timer(rows(300, 400))),
        (
            "the newest page of a whole read",
            Msg::RunsNewest(page(rows(300, 400), vec![])),
        ),
        ("the failure of the older runs", older_failed("reset")),
        (
            "the newest page of the next read",
            Msg::RunsNewest(page(rows(300, 400), vec![])),
        ),
    ] {
        app.update(msg);
        assert_eq!(bottom_two(&screen(&app)).0, REMOVING, "{name}");
    }
}
