//! The Runs screen: a status gutter, a one-line filter at the bottom (`/`), a scope toggle by
//! repository (`p`) and a refresh of the newest page every few seconds. The pure parts are `ham_core::runfilter`; these tests drive the
//! screen. While the filter line is open every character is the filter's (a `q` or an `n` too), Enter keeps the filter and gives the
//! keys back, Esc clears it.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::runfilter::{Refresh, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::style::Modifier;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";
const R4: &str = "01M5A00000000000000000R004";
const R5: &str = "01M5A00000000000000000R005";
const R6: &str = "01M5A00000000000000000R006";

fn run(id: &str, status: &str, target: Value, extra: Value) -> RunSummary {
    let mut v = json!({"runId": id, "status": status, "phase": "planning", "target": target,
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(v).unwrap()
}

fn repo(name: &str) -> Value {
    json!({"mode": "repo", "repo": format!("cc:{name}")})
}

fn waiting() -> Value {
    json!({"needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-16T09:00:00Z"}})
}

/// Six runs, newest first: done in calc-service, running in web-shop, waiting for an answer in web-shop, failed (cleared after an aborted
/// execution) in api-gateway, done with no repository, done in calc-service.
fn six() -> Vec<RunSummary> {
    vec![
        run(R1, "complete", repo("calc-service"), json!({})),
        run(R2, "active", repo("web-shop"), json!({})),
        run(R3, "active", repo("web-shop"), waiting()),
        run(
            R4,
            "cleared",
            repo("api-gateway"),
            json!({"aborted": {"status": "FAILED", "stoppedAt": "2026-10-16T08:30:00Z"}}),
        ),
        run(R5, "complete", json!({"mode": "s3-only"}), json!({})),
        run(R6, "complete", repo("calc-service"), json!({})),
    ]
}

fn runs_app(runs: Vec<RunSummary>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(runs)));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn draw(app: &App, w: u16, h: u16) -> (String, Terminal<TestBackend>) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    (out, t)
}

fn screen(app: &App) -> String {
    draw(app, 100, 24).0
}

fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    key(app, KeyCode::Char(c))
}

fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        let effects = ch(app, c);
        assert!(effects.is_empty(), "{c:?} caused {effects:?}");
    }
}

/// The screen row that holds `id`, without the frame.
fn row_of<'a>(text: &'a str, id: &str) -> Option<&'a str> {
    text.lines()
        .find(|l| l.contains(id))
        .map(|l| l.trim_start_matches('│'))
}

/// The ids of the runs listed, in order.
fn listed(text: &str) -> Vec<&'static str> {
    [R1, R2, R3, R4, R5, R6]
        .into_iter()
        .filter(|id| text.contains(id))
        .collect()
}

/// The bottom row with text (the key line or the filter line).
fn bottom(text: &str) -> &str {
    text.lines().rev().find(|l| !l.trim().is_empty()).unwrap()
}

#[test]
fn a_gutter_marks_running_waiting_and_failed_runs() {
    let app = runs_app(six());
    let text = screen(&app);
    let glyph = |id| row_of(&text, id).unwrap().chars().next().unwrap();
    assert_eq!(glyph(R1), ' ', "{text}");
    assert_eq!(glyph(R2), '>', "{text}");
    assert_eq!(glyph(R3), '!', "{text}");
    assert_eq!(glyph(R4), 'x', "{text}");
    assert_eq!(glyph(R5), ' ', "{text}");
}

#[test]
fn the_waiting_mark_is_bold_and_the_failed_one_is_not_plain() {
    let app = runs_app(six());
    let (text, t) = draw(&app, 100, 24);
    let buf = t.backend().buffer().clone();
    let y_of = |id: &str| text.lines().position(|l| l.contains(id)).unwrap() as u16;
    // The glyph is the first cell inside the frame.
    let waiting = &buf[(1, y_of(R3))];
    assert!(waiting.modifier.contains(Modifier::BOLD), "{waiting:?}");
    let failed = &buf[(1, y_of(R4))];
    assert_ne!(failed.fg, ratatui::style::Color::Reset, "{failed:?}");
}

#[test]
fn the_key_line_names_the_find_and_repository_keys() {
    let app = runs_app(six());
    let text = screen(&app);
    let line = bottom(&text);
    // The `p` key says what a press does (`p this repo` on this 100-column line; `tui_runs_key_label.rs` covers the other wordings).
    for want in ["/ find", "t tags", "p this repo", "r refresh", "q quit"] {
        assert!(line.contains(want), "{want:?} is not in {line:?}");
    }
}

#[test]
fn slash_opens_the_filter_line_and_words_narrow_the_list() {
    let mut app = runs_app(six());
    assert_eq!(listed(&screen(&app)).len(), 6);
    assert!(ch(&mut app, '/').is_empty());
    type_in(&mut app, "calc");
    let text = screen(&app);
    assert_eq!(listed(&text), [R1, R6], "{text}");
    // The filter line is the bottom row, and the count is under the list.
    assert!(bottom(&text).starts_with("/calc"), "{text}");
    assert!(text.contains("2 of 6 runs match \"calc\""), "{text}");
}

#[test]
fn the_cursor_sits_after_the_text_on_the_filter_line() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    let (text, mut t) = draw(&app, 100, 24);
    let pos = t.backend_mut().get_cursor_position().unwrap();
    let row = text.lines().position(|l| l.starts_with("/calc")).unwrap();
    assert_eq!((usize::from(pos.y), pos.x), (row, 5), "{text}");
}

#[test]
fn several_words_must_all_be_found_in_the_status_the_id_or_the_repository() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "web-shop active");
    assert_eq!(listed(&screen(&app)), [R2, R3]);
    type_in(&mut app, " needs");
    assert_eq!(listed(&screen(&app)), [R3]);
    // The id is searched too, and case does not matter.
    for _ in 0.." active needs".len() + "web-shop".len() + 1 {
        key(&mut app, KeyCode::Backspace);
    }
    type_in(&mut app, "r004");
    assert_eq!(listed(&screen(&app)), [R4]);
}

#[test]
fn a_filter_without_a_match_says_so_and_lists_nothing() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "zzz");
    let text = screen(&app);
    assert!(listed(&text).is_empty(), "{text}");
    assert!(text.contains("no run matches \"zzz\" (6 runs)"), "{text}");
    // Enter has nothing to open.
    assert!(key(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn every_character_is_the_filters_while_the_line_is_open_hotkeys_included() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    // `q` would quit, `n` open the composer, `r` reload, `j` and `k` move, `p` limit to a project.
    for c in ['q', 'n', 'r', 'j', 'k', 'p'] {
        assert!(ch(&mut app, c).is_empty(), "{c}");
    }
    assert_eq!(app.screen(), Screen::Runs);
    let text = screen(&app);
    assert!(bottom(&text).starts_with("/qnrjkp"), "{text}");
}

#[test]
fn enter_keeps_the_filter_and_gives_the_keys_back_and_esc_clears_it() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    assert!(key(&mut app, KeyCode::Enter).is_empty());
    let text = screen(&app);
    assert_eq!(listed(&text), [R1, R6], "{text}");
    // The filtered line reads `Esc clears · t tags · p <words>` (80 columns at most) so that `q quit` stays on an 80-column screen.
    assert!(bottom(&text).contains("Esc clears · t tags"), "{text}");
    // `j` moves inside the narrowed list: from R1 to R6, skipping the runs that are hidden.
    ch(&mut app, 'j');
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R6)),
        "{effects:?}"
    );
    // Back on the Runs screen, Esc clears the filter.
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Esc);
    let text = screen(&app);
    assert_eq!(listed(&text).len(), 6, "{text}");
    assert!(!text.contains("match \"calc\""), "{text}");
    // `n` is a new run again.
    ch(&mut app, 'n');
    assert_eq!(app.screen(), Screen::Composer);
}

#[test]
fn esc_while_typing_clears_the_filter_and_closes_the_line() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    assert!(bottom(&screen(&app)).starts_with("/calc"));
    assert!(key(&mut app, KeyCode::Esc).is_empty());
    let text = screen(&app);
    assert_eq!(listed(&text).len(), 6, "{text}");
    assert!(!bottom(&text).starts_with('/'), "{text}");
    assert_eq!(app.screen(), Screen::Runs);
    // The keys are the list's again.
    assert!(
        ch(&mut app, 'r')
            .iter()
            .any(|e| matches!(e, Effect::LoadRuns))
    );
}

#[test]
fn the_selection_follows_its_run_when_the_list_narrows() {
    let mut app = runs_app(six());
    // Select R6, the last run.
    for _ in 0..5 {
        ch(&mut app, 'j');
    }
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    key(&mut app, KeyCode::Enter);
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R6)),
        "R6 stayed selected: {effects:?}"
    );
}

#[test]
fn a_run_that_is_no_longer_shown_hands_the_selection_to_the_nearest_one() {
    let mut app = runs_app(six());
    // R3 is selected; only R1 and R6 match: the old position (2) is kept inside the new list, so R6.
    ch(&mut app, 'j');
    ch(&mut app, 'j');
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    key(&mut app, KeyCode::Enter);
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R6)),
        "{effects:?}"
    );
}

#[test]
fn what_matched_is_marked_in_the_row() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    // The TARGET column is on screen from 124 columns.
    let (text, t) = draw(&app, 130, 24);
    let buf = t.backend().buffer().clone();
    let y = text.lines().position(|l| l.contains(R1)).unwrap() as u16;
    let line = text.lines().nth(usize::from(y)).unwrap();
    // The target `cc:calc-service` has `calc` marked, and `cc:` before it is not.
    let at = line.find("cc:calc").unwrap();
    let x = line[..at].chars().count() as u16;
    let plain = &buf[(x, y)];
    let marked = &buf[(x + 3, y)];
    assert!(!plain.modifier.contains(Modifier::UNDERLINED), "{plain:?}");
    assert!(marked.modifier.contains(Modifier::UNDERLINED), "{marked:?}");
    assert!(buf[(x + 6, y)].modifier.contains(Modifier::UNDERLINED));
    assert!(!buf[(x + 7, y)].modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn p_limits_the_list_to_the_repository_of_the_selected_run_and_again_lifts_it() {
    let mut app = runs_app(six());
    // R2 is in web-shop.
    ch(&mut app, 'j');
    assert!(ch(&mut app, 'p').is_empty());
    let text = screen(&app);
    assert_eq!(listed(&text), [R2, R3], "{text}");
    assert!(text.contains("2 of 6 runs in cc:web-shop"), "{text}");
    ch(&mut app, 'p');
    assert_eq!(listed(&screen(&app)).len(), 6);
}

#[test]
fn p_on_a_list_without_runs_does_nothing() {
    let mut app = runs_app(vec![]);
    assert!(ch(&mut app, 'p').is_empty());
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn a_command_attempt_filters_nothing_and_enter_points_to_the_message_box() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "/runs");
    assert_eq!(listed(&screen(&app)).len(), 6);
    assert!(key(&mut app, KeyCode::Enter).is_empty());
    let text = screen(&app);
    assert!(text.contains("message box of a run"), "{text}");
    assert_eq!(listed(&text).len(), 6, "{text}");
}

// ---- the refresh ------------------------------------------------------------------------------------------------------------------

fn refreshes(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::RefreshRuns(Refresh::Timer)))
        .count()
}

#[test]
fn the_timer_reads_the_newest_page_every_few_seconds_and_never_two_at_once() {
    let mut app = runs_app(six());
    let mut asked = 0;
    for _ in 0..4 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 0, "not before the interval");
    asked += refreshes(&app.tick());
    assert_eq!(asked, 1, "the fifth tick asks");
    // While that read is in flight, no tick asks again.
    for _ in 0..20 {
        assert_eq!(refreshes(&app.tick()), 0);
    }
    // The answer arrives; the next interval asks again.
    app.update(Msg::RunsRead(Ok(RunsRead::First {
        runs: six(),
        has_more: false,
    })));
    let mut asked = 0;
    for _ in 0..5 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 1);
}

#[test]
fn a_failed_read_frees_the_timer_and_keeps_the_list() {
    let mut app = runs_app(six());
    for _ in 0..5 {
        app.tick();
    }
    app.update(Msg::RunsRead(Err("timeout".into())));
    let text = screen(&app);
    assert_eq!(listed(&text).len(), 6, "{text}");
    let mut asked = 0;
    for _ in 0..5 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 1, "the next interval asks again");
}

#[test]
fn the_timer_does_not_read_the_list_on_the_run_screen() {
    let mut app = runs_app(six());
    app.open_run(R1);
    let mut asked = 0;
    for _ in 0..20 {
        asked += refreshes(&app.tick());
    }
    assert_eq!(asked, 0);
}

#[test]
fn a_run_that_starts_to_wait_shows_its_mark_without_a_manual_refresh() {
    let mut app = runs_app(six());
    assert_eq!(row_of(&screen(&app), R2).unwrap().chars().next(), Some('>'));
    let mut newest = six();
    newest[1] = run(R2, "active", repo("web-shop"), waiting());
    // The newest page holds the first five; R6 is older than the page and stays as held.
    newest.truncate(5);
    app.update(Msg::RunsRead(Ok(RunsRead::First {
        runs: newest,
        has_more: true,
    })));
    let text = screen(&app);
    assert_eq!(
        row_of(&text, R2).unwrap().chars().next(),
        Some('!'),
        "{text}"
    );
    assert!(text.contains(R6), "the older run was kept: {text}");
}

#[test]
fn the_manual_read_replaces_the_list() {
    let mut app = runs_app(six());
    assert!(
        ch(&mut app, 'r')
            .iter()
            .any(|e| matches!(e, Effect::LoadRuns))
    );
    app.update(Msg::RunsRead(Ok(RunsRead::All(six()[..2].to_vec()))));
    assert_eq!(listed(&screen(&app)), [R1, R2]);
}

#[test]
fn the_filter_survives_a_refresh_and_the_selection_stays_on_its_run() {
    let mut app = runs_app(six());
    ch(&mut app, '/');
    type_in(&mut app, "calc");
    key(&mut app, KeyCode::Enter);
    ch(&mut app, 'j'); // R6
    app.update(Msg::RunsRead(Ok(RunsRead::First {
        runs: six(),
        has_more: false,
    })));
    let text = screen(&app);
    assert_eq!(listed(&text), [R1, R6], "{text}");
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R6)),
        "{effects:?}"
    );
    assert_eq!(app.screen(), Screen::Dashboard);
}
