//! Prompt history and Ctrl-R in the message box of the run screen. Up at the first
//! row and Down at the last row of the box step through the lines that were sent (newest first), then the run's earlier messages
//! (its prompt); Down past the newest gives the draft back. A recalled text is tainted (it never runs a command at once), Esc on it
//! gives the draft back, and Ctrl-R searches the same lines. Nothing is written to disk.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::*;
use crossterm::event::{KeyCode, KeyModifiers};
use ham::tui::ext::{Done, Job};
use ham::tui::{App, Effect, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, Refresh, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::style::Modifier;
use serde_json::json;

fn up(app: &mut ham::tui::App, clock: &mut Clock) {
    press(app, clock, KeyCode::Up, 60);
}

fn down(app: &mut ham::tui::App, clock: &mut Clock) {
    press(app, clock, KeyCode::Down, 60);
}

#[test]
fn up_recalls_the_lines_that_were_sent_newest_first_and_down_gives_the_draft_back() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    send(&mut app, &mut clock, "/doctor");
    type_text(&mut app, &mut clock, "draft in progress");
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/doctor"));
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/help"));
    down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/doctor"));
    down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("draft in progress"));
    assert!(
        !app.draft_is_tainted(),
        "the draft comes back with the taint it had"
    );
}

#[test]
fn a_message_sent_to_cline_is_recalled_too() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    let effects = send(&mut app, &mut clock, "please add tests");
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, ham::tui::Effect::Answer { .. })),
        "{effects:?}"
    );
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("please add tests"));
}

#[test]
fn up_inside_a_many_line_draft_moves_the_caret_and_only_the_first_row_recalls() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    type_text(&mut app, &mut clock, "a");
    press_mod(
        &mut app,
        &mut clock,
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
        60,
    );
    type_text(&mut app, &mut clock, "b");
    assert_eq!(app.chat_text().as_deref(), Some("a\nb"));
    // The caret is on the second line: Up moves it to the first, the text stays.
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("a\nb"));
    type_text(&mut app, &mut clock, "X");
    assert_eq!(app.chat_text().as_deref(), Some("aX\nb"));
    // On the first row Up recalls.
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/help"));
    // Down on its last row gives the draft back.
    down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("aX\nb"));
}

#[test]
fn a_recalled_command_is_tainted_and_shows_its_preview_before_it_runs() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/doctor");
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/doctor"));
    assert!(app.draft_is_tainted());
    clock.after(400);
    let effects = enter(&mut app, &mut clock);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, ham::tui::Effect::RunCommand { .. })),
        "a recalled line does not run at once: {effects:?}"
    );
    assert!(
        screen(&app).contains("Press Enter to run it"),
        "{}",
        screen(&app)
    );
}

#[test]
fn esc_on_a_recalled_text_gives_the_draft_back_and_stays_in_the_box() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    type_text(&mut app, &mut clock, "mine");
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("/help"));
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.chat_text().as_deref(), Some("mine"));
    assert_eq!(app.mode(), ham::tui::keys::Mode::Typing);
    assert!(!app.draft_is_tainted());
}

#[test]
fn the_runs_prompt_is_in_the_history() {
    let (mut app, mut clock) = ready();
    up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(PROMPT));
}

#[test]
fn up_with_nothing_older_leaves_the_box_alone() {
    let (mut app, mut clock) = ready();
    up(&mut app, &mut clock);
    up(&mut app, &mut clock);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(PROMPT),
        "the oldest stays on screen"
    );
}

// ---- Ctrl-R ------------------------------------------------------------------------------------------------------------------------

#[test]
fn ctrl_r_searches_the_history_enter_takes_the_match_and_the_text_is_tainted() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    send(&mut app, &mut clock, "/doctor");
    ctrl(&mut app, &mut clock, 'r');
    let text = screen(&app);
    assert!(text.contains("history search:"), "{text}");
    for c in "hel".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    let text = screen(&app);
    assert!(text.contains("history search: hel  1/1"), "{text}");
    assert!(text.contains("/help"), "the match is shown:\n{text}");
    // The letters went to the search, not to the box.
    assert_eq!(app.chat_text().as_deref(), Some(""));
    press(&mut app, &mut clock, KeyCode::Enter, 60);
    assert_eq!(app.chat_text().as_deref(), Some("/help"));
    assert!(app.draft_is_tainted());
    assert!(!screen(&app).contains("history search:"));
}

#[test]
fn ctrl_r_again_finds_the_next_older_match_and_backspace_widens_the_search() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    send(&mut app, &mut clock, "/doctor");
    send(&mut app, &mut clock, "/help me");
    ctrl(&mut app, &mut clock, 'r');
    for c in "help".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    assert!(
        screen(&app).contains("history search: help  1/2"),
        "{}",
        screen(&app)
    );
    ctrl(&mut app, &mut clock, 'r');
    assert!(
        screen(&app).contains("history search: help  2/2"),
        "{}",
        screen(&app)
    );
    for _ in 0..4 {
        press(&mut app, &mut clock, KeyCode::Backspace, 60);
    }
    for c in "doc".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    assert!(
        screen(&app).contains("history search: doc  1/1"),
        "{}",
        screen(&app)
    );
}

#[test]
fn esc_in_the_search_gives_the_draft_back() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    type_text(&mut app, &mut clock, "mine");
    ctrl(&mut app, &mut clock, 'r');
    for c in "hel".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.chat_text().as_deref(), Some("mine"));
    assert_eq!(app.mode(), ham::tui::keys::Mode::Typing);
    assert!(!screen(&app).contains("history search:"));
    assert!(!app.draft_is_tainted());
}

#[test]
fn a_search_with_no_match_says_so_and_enter_changes_nothing() {
    let (mut app, mut clock) = ready();
    send(&mut app, &mut clock, "/help");
    type_text(&mut app, &mut clock, "mine");
    ctrl(&mut app, &mut clock, 'r');
    for c in "zzz".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    assert!(
        screen(&app).contains("history search: zzz  no match"),
        "{}",
        screen(&app)
    );
    press(&mut app, &mut clock, KeyCode::Enter, 60);
    assert_eq!(app.chat_text().as_deref(), Some("mine"));
}

// ---- the prompts of earlier runs -------------------------------------------------------------------------------------------

const OLD1: &str = "01K6A0000000000000000000O1";
const OLD2: &str = "01K6A0000000000000000000O2";
const NEW3: &str = "01K6A0000000000000000000O3";
/// The prompt of run NEW3: its first line is longer than the 80 characters the run index keeps, so the list shows it cut.
const FULL3: &str = "Third run: add retries to the uploader and write the tests first, then document the retry policy in the README so that\nthe team can read it";

fn title3() -> String {
    ham_core::run_index::title_of(FULL3)
}

fn run_summary(id: &str, created: &str) -> RunSummary {
    serde_json::from_value(json!({
        "runId": id, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": created, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"
    }))
    .unwrap()
}

/// The Runs list as the app reads it: every row with its run-index entry (the title), in the order given.
fn list(app: &mut App, rows: &[(&str, &str, Option<&str>)]) {
    let runs = rows.iter().map(|(id, at, _)| run_summary(id, at)).collect();
    let index = rows
        .iter()
        .filter_map(|(id, _, title)| {
            title.map(|t| IndexEntry {
                run_id: (*id).to_string(),
                title: Some(t.to_string()),
                model: None,
                name: None,
                name_source: None,
                tags: None,
            })
        })
        .collect();
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index,
    })));
}

/// The list of the tests below, newest run first, and the run on screen (RUN) among them with the title of its own prompt.
fn the_list(app: &mut App) {
    list(
        app,
        &[
            (RUN, "2026-10-14T20:00:00Z", Some(PROMPT)),
            (NEW3, "2026-10-13T09:00:00Z", Some(&title3())),
            (OLD2, "2026-10-12T09:00:00Z", Some("Second run prompt")),
            (OLD1, "2026-10-10T09:00:00Z", Some("Oldest run prompt")),
        ],
    );
}

fn press_up(app: &mut App, clock: &mut Clock) -> Vec<Effect> {
    press(app, clock, KeyCode::Up, 60)
}

fn press_down(app: &mut App, clock: &mut Clock) -> Vec<Effect> {
    press(app, clock, KeyCode::Down, 60)
}

fn fetch(run: &str) -> Effect {
    Effect::Ext(Job::FullPrompt {
        run_id: run.to_string(),
    })
}

fn fetches(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Ext(Job::FullPrompt { .. })))
        .count()
}

fn arrives(app: &mut App, run: &str, full: &str) {
    app.update(Msg::Ext(Done::FullPrompt {
        run_id: run.to_string(),
        result: Ok(full.to_string()),
    }));
}

fn answers(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::Answer { .. }))
}

#[test]
fn up_reaches_the_prompts_of_earlier_runs_after_the_runs_own_prompt_newest_first() {
    let (mut app, mut clock) = ready();
    // Out of order on purpose: the app puts the newest first.
    list(
        &mut app,
        &[
            (OLD1, "2026-10-10T09:00:00Z", Some("Oldest run prompt")),
            (RUN, "2026-10-14T20:00:00Z", Some(PROMPT)),
            (NEW3, "2026-10-13T09:00:00Z", Some(&title3())),
            (OLD2, "2026-10-12T09:00:00Z", Some("Second run prompt")),
        ],
    );
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(PROMPT),
        "the run's own prompt first"
    );
    assert_eq!(fetches(&fx), 0, "it is complete: nothing to fetch");
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    assert_eq!(
        fx,
        vec![fetch(NEW3)],
        "a title is fetched in full when it is recalled"
    );
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("Second run prompt"));
    assert_eq!(fx, vec![fetch(OLD2)]);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("Oldest run prompt"));
    assert_eq!(fx, vec![fetch(OLD1)]);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(
        app.chat_text().as_deref(),
        Some("Oldest run prompt"),
        "the oldest stays"
    );
    assert!(fx.is_empty(), "{fx:?}");
}

#[test]
fn the_run_on_screen_is_not_listed_among_the_earlier_runs() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    // Five Ups, every text kept: the run's own prompt once (its title in the list is the same text and would show a second time if the
    // run were listed), the three earlier runs, and the oldest staying on screen.
    let mut seen = Vec::new();
    for _ in 0..5 {
        press_up(&mut app, &mut clock);
        seen.push(app.chat_text().unwrap());
    }
    assert_eq!(
        seen,
        [
            PROMPT,
            &title3(),
            "Second run prompt",
            "Oldest run prompt",
            "Oldest run prompt"
        ]
    );
}

#[test]
fn down_walks_back_through_the_earlier_runs_and_gives_the_draft_back() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    type_text(&mut app, &mut clock, "my draft");
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("Second run prompt"));
    assert_eq!(fx, vec![fetch(OLD2)]);
    let fx = press_down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    assert_eq!(fetches(&fx), 0, "its fetch is already out: not asked twice");
    press_down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(PROMPT));
    press_down(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("my draft"));
    assert!(!app.draft_is_tainted());
}

#[test]
fn a_list_whose_rows_have_no_title_adds_nothing_to_the_history() {
    let (mut app, mut clock) = ready();
    list(
        &mut app,
        &[
            (RUN, "2026-10-14T20:00:00Z", None),
            (OLD1, "2026-10-10T09:00:00Z", None),
        ],
    );
    press_up(&mut app, &mut clock);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(PROMPT));
    assert!(fx.is_empty());
}

#[test]
fn enter_waits_while_the_full_prompt_is_on_its_way_and_the_wait_ends() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    clock.after(400);
    let fx = enter(&mut app, &mut clock);
    assert!(!answers(&fx), "the short form is never sent: {fx:?}");
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    assert!(
        screen(&app).contains("full prompt"),
        "the screen says why:\n{}",
        screen(&app)
    );
    // Nine seconds of waiting: still waiting. The tenth lifts the wait and says so.
    for _ in 0..9 {
        app.tick();
    }
    clock.after(400);
    assert!(!answers(&enter(&mut app, &mut clock)));
    app.tick();
    app.tick();
    let text = screen(&app);
    assert!(
        text.contains("did not arrive"),
        "the wait ended and the screen says so:\n{text}"
    );
    // The box is not hung, and the short form is not sent by a stray Enter: the first Enter after the wait says what a second one does.
    clock.after(400);
    let fx = enter(&mut app, &mut clock);
    assert!(!answers(&fx), "{fx:?}");
    assert!(screen(&app).contains("Enter again"), "{}", screen(&app));
    clock.after(400);
    let fx = enter(&mut app, &mut clock);
    assert!(answers(&fx), "the second Enter sends it as it is: {fx:?}");
}

#[test]
fn the_full_prompt_replaces_the_title_when_it_arrives_and_then_enter_sends_it() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    arrives(&mut app, NEW3, FULL3);
    assert_eq!(app.chat_text().as_deref(), Some(FULL3));
    assert!(app.draft_is_tainted(), "a recalled text is tainted");
    clock.after(400);
    let fx = enter(&mut app, &mut clock);
    assert!(answers(&fx), "{fx:?}");
}

#[test]
fn a_box_that_was_edited_keeps_its_edit_when_the_full_prompt_arrives() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    type_text(&mut app, &mut clock, " and more");
    let edited = format!("{} and more", title3());
    assert_eq!(app.chat_text().as_deref(), Some(edited.as_str()));
    arrives(&mut app, NEW3, FULL3);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(edited.as_str()),
        "the edit is the person's: the full prompt does not replace it"
    );
}

#[test]
fn esc_gives_the_draft_back_and_a_late_full_prompt_changes_nothing() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    type_text(&mut app, &mut clock, "mine");
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    assert_eq!(app.chat_text().as_deref(), Some("mine"));
    arrives(&mut app, NEW3, FULL3);
    assert_eq!(app.chat_text().as_deref(), Some("mine"));
    assert!(!app.draft_is_tainted());
}

#[test]
fn a_failed_fetch_ends_the_wait_and_says_why() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    app.update(Msg::Ext(Done::FullPrompt {
        run_id: NEW3.to_string(),
        result: Err("the stage said 503".to_string()),
    }));
    let text = screen(&app);
    assert!(text.contains("the stage said 503"), "{text}");
    assert!(text.contains("full prompt"), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    clock.after(400);
    assert!(
        !answers(&enter(&mut app, &mut clock)),
        "the first Enter asks"
    );
    assert!(screen(&app).contains("Enter again"), "{}", screen(&app));
    clock.after(400);
    assert!(answers(&enter(&mut app, &mut clock)), "the box is not hung");
}

#[test]
fn a_prompt_that_is_not_the_runs_ends_the_wait_with_a_notice() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    arrives(
        &mut app,
        NEW3,
        "Something else entirely\nnot that run's prompt",
    );
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    let text = screen(&app);
    assert!(text.contains("full prompt"), "{text}");
    clock.after(400);
    assert!(
        !answers(&enter(&mut app, &mut clock)),
        "the first Enter asks"
    );
    clock.after(400);
    assert!(answers(&enter(&mut app, &mut clock)), "the box is not hung");
}

#[test]
fn an_edit_after_a_failed_wait_sends_with_one_enter() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    app.update(Msg::Ext(Done::FullPrompt {
        run_id: NEW3.to_string(),
        result: Err("the stage said 503".to_string()),
    }));
    // The person completes the text themselves: it is their text now, and one Enter sends it.
    type_text(&mut app, &mut clock, " and the rest");
    let fx = enter(&mut app, &mut clock);
    assert!(answers(&fx), "{fx:?}");
}

#[test]
fn the_next_recall_asks_again_after_a_wait_that_ran_out() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(fx, vec![fetch(NEW3)]);
    // The request never comes back; the wait runs out.
    for _ in 0..11 {
        app.tick();
    }
    assert!(screen(&app).contains("did not arrive"), "{}", screen(&app));
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    press_up(&mut app, &mut clock);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    assert_eq!(
        fx,
        vec![fetch(NEW3)],
        "the old request is not waited for again"
    );
}

#[test]
fn ctrl_r_searches_the_prompts_of_earlier_runs_and_a_match_is_fetched_in_full() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    ctrl(&mut app, &mut clock, 'r');
    for c in "retries".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    let text = screen(&app);
    assert!(text.contains("history search: retries  1/1"), "{text}");
    assert!(text.contains("Third run: add retries"), "{text}");
    let fx = press(&mut app, &mut clock, KeyCode::Enter, 60);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
    assert_eq!(fx, vec![fetch(NEW3)]);
    arrives(&mut app, NEW3, FULL3);
    assert_eq!(app.chat_text().as_deref(), Some(FULL3));
}

#[test]
fn a_prompt_that_was_fetched_stays_when_the_list_is_read_again() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    arrives(&mut app, NEW3, FULL3);
    press(&mut app, &mut clock, KeyCode::Esc, 60);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    let fx = press_up(&mut app, &mut clock);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(FULL3),
        "the full prompt is kept"
    );
    assert!(fx.is_empty(), "nothing to fetch twice: {fx:?}");
}

#[test]
fn a_chat_opened_without_the_runs_screen_asks_for_the_list_once() {
    let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.open_run(RUN);
    assert_eq!(
        app.load_earlier_runs(),
        vec![Effect::RefreshRuns(Refresh::Timer)]
    );
    assert!(app.load_earlier_runs().is_empty(), "the read is out");
    the_list(&mut app);
    assert!(app.load_earlier_runs().is_empty(), "the list is held");
    // The answer fed the history: the earlier runs are one Up away (typing mode, the run's own prompt is not known yet).
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    clock.after(5_000);
    press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some(title3().as_str()));
}

#[test]
fn the_earlier_runs_follow_the_run_that_is_on_screen() {
    let (mut app, mut clock) = ready();
    the_list(&mut app);
    // Open another run from the list: its own title leaves the earlier runs and the first run's joins them.
    app.open_run(NEW3);
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    clock.after(5_000);
    let mut seen = Vec::new();
    for _ in 0..6 {
        press_up(&mut app, &mut clock);
        let t = app.chat_text().unwrap();
        if seen.last() != Some(&t) {
            seen.push(t);
        }
    }
    assert_eq!(
        seen,
        [PROMPT, "Second run prompt", "Oldest run prompt"],
        "RUN's prompt is an earlier run now, NEW3's is not listed"
    );
}

#[test]
fn a_paste_into_a_recalled_title_ends_the_wait_and_keeps_the_paste() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    app.handle_paste(" and some pasted words".to_string());
    let edited = format!("{} and some pasted words", title3());
    assert_eq!(app.chat_text().as_deref(), Some(edited.as_str()));
    // The box is the person's text now: nothing waits, and the full prompt that arrives later does not replace it.
    clock.after(400);
    assert!(
        answers(&enter(&mut app, &mut clock)),
        "the paste ended the wait"
    );
    arrives(&mut app, NEW3, FULL3);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(""),
        "sent, and the late answer did not refill the box"
    );
}

#[test]
fn a_wait_does_not_follow_the_person_to_another_run() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    the_list(&mut app);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    press_up(&mut app, &mut clock);
    assert_eq!(app.chat_text().as_deref(), Some("Second run prompt"));
    // Another run is opened while the full prompt is on its way; its box holds nothing of the walk.
    app.open_run(NEW3);
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    clock.after(5_000);
    clock.after(400);
    enter(&mut app, &mut clock);
    assert!(
        !screen(&app).contains("full prompt"),
        "the wait belonged to the other run:\n{}",
        screen(&app)
    );
    // The answer arrives while the other run is on screen: its box is not touched.
    arrives(&mut app, OLD2, "Second run prompt\nand its second line");
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

// ---- the query marked in the Ctrl-R match -----------------------------------------------------------------------------------

/// The row under the bar of the Ctrl-R search at `width`: each cell's symbol and whether it is reversed.
fn match_row(app: &App, width: u16) -> Vec<(String, bool)> {
    let buf = buffer(app, width, 30);
    let row = |y: u16| -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    };
    let bar = (0..30)
        .find(|&y| row(y).contains("  1/1") || row(y).contains("no match"))
        .expect("the bar of the search is on the screen");
    (0..width)
        .map(|x| {
            let c = &buf[(x, bar + 1)];
            (
                c.symbol().to_string(),
                c.modifier.contains(Modifier::REVERSED),
            )
        })
        .collect()
}

fn row_text(row: &[(String, bool)]) -> String {
    row.iter()
        .map(|(s, _)| s.as_str())
        .collect::<String>()
        .trim()
        .to_string()
}

fn marked_text(row: &[(String, bool)]) -> String {
    row.iter()
        .filter(|(_, m)| *m)
        .map(|(s, _)| s.as_str())
        .collect()
}

const SENT: &str = "Run the checks, then the CHECKS again";

fn searching(query: &str) -> App {
    let (mut app, mut clock) = ready_with(vec![question()]);
    send(&mut app, &mut clock, SENT);
    ctrl(&mut app, &mut clock, 'r');
    for c in query.chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    app
}

#[test]
fn ctrl_r_marks_each_occurrence_of_the_query_in_the_match_row_at_three_widths() {
    for query in ["checks", "CHECKS", "ChEcKs"] {
        let app = searching(query);
        for width in [100u16, 50] {
            let row = match_row(&app, width);
            assert_eq!(row_text(&row), SENT, "width {width}");
            assert_eq!(
                marked_text(&row),
                "checksCHECKS",
                "width {width}, query {query}: both occurrences, whatever the letter case"
            );
        }
        // Narrow: the row is cut around the first occurrence, which stays marked.
        let row = match_row(&app, 24);
        let text = row_text(&row);
        assert!(text.chars().count() <= 24, "{text}");
        assert!(
            text.starts_with('\u{2026}') && text.ends_with('\u{2026}'),
            "{text}"
        );
        assert_eq!(
            marked_text(&row),
            "checks",
            "width 24, query {query}: {text}"
        );
    }
}

#[test]
fn the_marks_are_a_style_attribute_not_only_a_colour() {
    let app = searching("checks");
    let row = match_row(&app, 100);
    // The reversed cells are exactly the letters of the query; every other letter of the row is plain.
    let plain = row.iter().filter(|(s, m)| !*m && s != " ").count();
    let reversed = row.iter().filter(|(_, m)| *m).count();
    assert_eq!(reversed, 12, "checks and CHECKS");
    assert_eq!(plain, SENT.replace(' ', "").chars().count() - 12);
}

#[test]
fn a_match_with_a_line_break_keeps_its_marks_after_the_symbol_that_shows_it() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    type_text(&mut app, &mut clock, "first line");
    press_mod(
        &mut app,
        &mut clock,
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
        60,
    );
    type_text(&mut app, &mut clock, "the checks here");
    enter(&mut app, &mut clock);
    clock.after(400);
    ctrl(&mut app, &mut clock, 'r');
    for c in "checks".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    for width in [100u16, 50, 30] {
        let row = match_row(&app, width);
        assert_eq!(
            marked_text(&row),
            "checks",
            "width {width}: {}",
            row_text(&row)
        );
    }
    assert_eq!(
        row_text(&match_row(&app, 100)),
        "first line \u{23ce} the checks here"
    );
}

#[test]
fn no_query_and_no_match_mark_nothing() {
    let (mut app, mut clock) = ready_with(vec![question()]);
    send(&mut app, &mut clock, SENT);
    ctrl(&mut app, &mut clock, 'r');
    for c in "zzz".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    let row = match_row(&app, 100);
    assert_eq!(marked_text(&row), "");
    assert_eq!(row_text(&row), "");
}

#[test]
fn a_long_prompt_keeps_the_query_in_view_at_a_narrow_width() {
    let long = format!("{}needle{}", "word ".repeat(30), " tail".repeat(5));
    let (mut app, mut clock) = ready_with(vec![question()]);
    send(&mut app, &mut clock, &long);
    ctrl(&mut app, &mut clock, 'r');
    for c in "needle".chars() {
        press(&mut app, &mut clock, KeyCode::Char(c), 60);
    }
    for width in [100u16, 40, 24] {
        let row = match_row(&app, width);
        let text = row_text(&row);
        assert!(text.chars().count() <= usize::from(width), "{text}");
        assert_eq!(marked_text(&row), "needle", "width {width}: {text}");
    }
}

// ---- nothing on disk ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_screen_code_that_holds_the_history_does_no_file_access() {
    // The ring lives in memory for the life of the process (invariant 5): neither the app nor the history module opens a file.
    for (name, src) in [
        ("app.rs", include_str!("../src/tui/app.rs")),
        ("earlier.rs", include_str!("../src/tui/earlier.rs")),
        ("matchrow.rs", include_str!("../src/tui/matchrow.rs")),
        ("history.rs", include_str!("../../ham-core/src/history.rs")),
    ] {
        for needle in [
            "std::fs",
            "File::",
            "OpenOptions",
            "fs::write",
            "create_dir",
        ] {
            assert!(!src.contains(needle), "{name} names {needle}");
        }
    }
}
