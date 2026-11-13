//! Open anything and the sessions picker. One box searches the runs you have, their
//! repositories and what was said in them; the results are grouped by repository with the state, the title and a snippet; Enter opens the
//! run at the place of the match. `/sessions` is the Runs screen. The search of chat contents
//! is the route `GET /v1/search`, asked after the words stop changing; the held list answers the rest at once.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::{Clock, screen as shot, type_text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::{Done, Job};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::runsearch::SearchResponse;
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";

fn run(id: &str, status: &str, repo: &str, at: &str) -> RunSummary {
    serde_json::from_value(json!({"runId": id, "status": status, "phase": "planning",
        "target": {"mode": "repo", "repo": format!("cc:{repo}")}, "createdAt": at, "ownerPrincipalArn": ARN}))
    .unwrap()
}

fn app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(vec![
        run(R1, "complete", "calc-service", "2026-10-16T08:00:00Z"),
        run(R2, "active", "web-shop", "2026-10-15T08:00:00Z"),
        run(R3, "complete", "calc-service", "2026-10-14T08:00:00Z"),
    ])));
    app
}

fn ctrl_o(app: &mut App, clock: &mut Clock) -> Vec<Effect> {
    let at = clock.after(300);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), at)
}

fn searched(query: &str, hits: Value) -> Msg {
    let r: SearchResponse = serde_json::from_value(
        json!({"hits": hits, "searched": 3, "partial": false, "more": false, "partialCoverage": 0}),
    )
    .unwrap();
    Msg::Ext(Done::Searched {
        query: query.into(),
        result: Ok(r),
    })
}

fn search_jobs(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(Job::Search { query }) => Some(query.q.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn ctrl_o_opens_the_box_and_the_words_list_runs_and_repositories_from_the_held_list_at_once() {
    let mut app = app();
    let mut clock = Clock::new();
    ctrl_o(&mut app, &mut clock);
    assert!(
        shot(&app, 100, 30).contains("Open anything"),
        "{}",
        shot(&app, 100, 30)
    );
    type_text(&mut app, &mut clock, "calc");
    let text = shot(&app, 100, 30);
    assert!(
        text.contains("cc:calc-service"),
        "the repository is a group: {text}"
    );
    assert!(text.contains(R1) && text.contains(R3), "{text}");
    assert!(!text.contains(R2), "web-shop does not match: {text}");
}

#[test]
fn the_chat_search_is_asked_once_the_words_stop_changing() {
    let mut app = app();
    let mut clock = Clock::new();
    ctrl_o(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "percent change");
    let mut asked = Vec::new();
    for _ in 0..4 {
        asked.extend(search_jobs(&app.tick()));
    }
    assert_eq!(asked, ["percent change"], "once, with the words as typed");
    // The same words again are not asked again.
    for _ in 0..4 {
        assert!(search_jobs(&app.tick()).is_empty());
    }
}

#[test]
fn a_chat_hit_is_listed_with_its_snippet_under_its_repository_and_enter_opens_the_run_at_the_match()
{
    let mut app = app();
    let mut clock = Clock::new();
    ctrl_o(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "percent");
    app.update(searched(
        "percent",
        json!([{"runId": R2, "createdAt": "2026-10-15T08:00:00Z", "field": "chat",
                "snippet": "\u{2026}added the percent_change helper\u{2026}", "seq": 3, "offset": 5, "open": "open-token-7", "coverage": "full"}]),
    ));
    let text = shot(&app, 100, 30);
    let group = text.find("cc:web-shop").expect(&text);
    let snippet = text.find("added the percent_change helper").expect(&text);
    assert!(group < snippet, "{text}");
    assert!(text.contains(R2), "{text}");
    // Enter opens the run, asking the transcript from the match.
    let at = clock.after(300);
    let effects = app.handle_key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), at);
    assert_eq!(app.screen(), Screen::Dashboard);
    assert_eq!(app.focused_run(), Some(R2));
    let q = effects
        .iter()
        .find_map(|e| match e {
            Effect::Ext(Job::Transcript { query, read, .. }) => Some((query.clone(), *read)),
            _ => None,
        })
        .expect("the transcript is asked from the match");
    assert_eq!(q.0.after.as_deref(), Some("open-token-7"));
    assert_eq!(q.1, ham_core::stored::Read::First);
}

#[test]
fn esc_closes_the_box_and_the_arrow_keys_move_the_mark() {
    let mut app = app();
    let mut clock = Clock::new();
    ctrl_o(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "calc");
    let at = clock.after(100);
    app.handle_key_at(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), at);
    let marked = |s: &str| {
        s.lines()
            .filter(|l| l.contains('>') && l.contains("01M5A"))
            .count()
    };
    assert_eq!(marked(&shot(&app, 100, 30)), 1);
    let at = clock.after(100);
    app.handle_key_at(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), at);
    assert!(!shot(&app, 100, 30).contains("Open anything"));
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn slash_open_with_words_opens_the_box_and_with_a_run_id_still_opens_the_run() {
    let (mut app, _) = chatfx::ready_with(chatfx::base_events());
    let mut clock = Clock::new();
    chatfx::send(&mut app, &mut clock, "/open percent change");
    let text = shot(&app, 100, 30);
    assert!(text.contains("Open anything"), "{text}");
    assert!(
        text.contains("percent change"),
        "the words are in the box: {text}"
    );
    let (mut app, _) = chatfx::ready_with(chatfx::base_events());
    let mut clock = Clock::new();
    let effects = chatfx::send(&mut app, &mut clock, &format!("/open {R2}"));
    assert_eq!(app.focused_run(), Some(R2), "{effects:?}");
}

#[test]
fn slash_sessions_shows_the_runs_screen() {
    let (mut app, _) = chatfx::ready_with(chatfx::base_events());
    let mut clock = Clock::new();
    let effects = chatfx::send(&mut app, &mut clock, "/sessions");
    assert_eq!(app.screen(), Screen::Runs);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::LoadRuns)),
        "{effects:?}"
    );
}
