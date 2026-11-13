//! In the terminal app, without a terminal: the Runs list with a BACKEND column and a banner for each backend that did not answer, the New
//! run screen with its Backend box (Tab order, the arrow keys, the refusal of a start with no pick and of a customer-content start on a backend
//! not declared safe, the effect of a start on one backend), and a client with one backend, whose screens hold none of it.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, OwnerLookup, Screen};
use ham_core::backends::{BackendInfo, BackendView, Health};
use ham_core::runfilter::{IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::Arc;

fn info(alias: &str, health: Health, safe: bool) -> BackendInfo {
    BackendInfo {
        alias: alias.into(),
        health,
        version: Some("5.05".into()),
        stale: false,
        customer_content: safe,
        profile: Some(alias.into()),
    }
}

fn view() -> BackendView {
    BackendView {
        backends: vec![
            info("work", Health::Up, true),
            info(
                "spare",
                Health::SignedOut {
                    fix: "aws sso login --profile spare".into(),
                },
                false,
            ),
            info(
                "lab",
                Health::Down("network: connection refused".into()),
                false,
            ),
        ],
    }
}

fn summary(id: &str, at: &str) -> RunSummary {
    serde_json::from_value(serde_json::json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": at, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"}))
    .unwrap()
}

fn app(pick: Option<&str>) -> App {
    let mut app = App::new_direct(
        "nonce-1".into(),
        None,
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    );
    app.set_backends(
        OwnerLookup(Arc::new(|id| {
            Some(if id.ends_with('1') { "work" } else { "spare" }.to_string())
        })),
        pick.map(str::to_string),
    );
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![
            summary("01K6A0000000000000000000R2", "2026-11-04T10:00:09Z"),
            summary("01K6A0000000000000000000R1", "2026-11-04T10:00:05Z"),
        ]),
        index: Vec::new(),
    })));
    app.update(Msg::Backends(Some(view())));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn ctrl_s(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

fn type_text(app: &mut App, s: &str) {
    for ch in s.chars() {
        press(app, KeyCode::Char(ch));
    }
}

#[test]
fn b584_the_runs_list_has_a_backend_column_and_a_banner_for_each_backend_that_did_not_answer() {
    let app = app(None);
    for (w, h) in [(80u16, 24u16), (120, 30), (200, 40)] {
        let text = screen(&app, w, h);
        assert!(text.contains("BACKEND"), "{w}x{h}\n{text}");
        let r2 = text.lines().find(|l| l.contains("R2")).unwrap_or_default();
        assert!(r2.contains("spare"), "{w}x{h}: {r2}");
        let r1 = text.lines().find(|l| l.contains("R1")).unwrap_or_default();
        assert!(r1.contains("work"), "{w}x{h}: {r1}");
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("spare: sign in needed") || flat.contains("spare: sign in"),
            "{w}x{h}\n{text}"
        );
        assert!(
            flat.contains("lab: not reachable") || flat.contains("lab: not"),
            "{w}x{h}\n{text}"
        );
    }
}

#[test]
fn b584_a_client_with_one_backend_shows_no_column_no_banner_and_no_box() {
    let mut app = App::new_direct(
        "nonce-1".into(),
        None,
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    );
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![summary(
            "01K6A0000000000000000000R1",
            "2026-11-04T10:00:05Z",
        )]),
        index: Vec::new(),
    })));
    app.update(Msg::Backends(None));
    let text = screen(&app, 120, 30);
    assert!(
        !text.contains("BACKEND") && !text.contains("not reachable"),
        "{text}"
    );
    press(&mut app, KeyCode::Char('n'));
    assert!(!screen(&app, 120, 40).contains("Backend ("));
}

#[test]
fn b584_the_composer_tabs_through_the_backend_box_and_the_arrows_pick_a_backend() {
    let mut app = app(None);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 150, 50);
    assert!(text.contains("Backend (←→ picks for this run)"), "{text}");
    assert!(text.contains("none chosen"), "{text}");
    // Prompt, Target, Repo, Base, Options, Backend.
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab);
    }
    press(&mut app, KeyCode::Right);
    assert!(screen(&app, 150, 50).contains("(•) work"));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Down);
    assert!(screen(&app, 150, 50).contains("(•) lab"));
    press(&mut app, KeyCode::Right);
    assert!(screen(&app, 150, 50).contains("(•) work"), "it goes round");
    press(&mut app, KeyCode::Left);
    assert!(screen(&app, 150, 50).contains("(•) lab"));
    press(&mut app, KeyCode::Up);
    assert!(screen(&app, 150, 50).contains("(•) spare"));
    // Shift-Tab goes back to the Options box, and Tab twice reaches the Model box past the Backend box.
    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    assert!(screen(&app, 150, 50).contains("↑↓ pick model"));
}

fn s3_only(app: &mut App) {
    press(app, KeyCode::Char('n'));
    type_text(app, "add a test");
    press(app, KeyCode::Tab);
    press(app, KeyCode::Right);
    press(app, KeyCode::Right);
}

#[test]
fn b584_a_start_with_no_backend_picked_is_refused_on_the_form_and_a_pick_sends_it_to_that_backend()
{
    let mut app = app(None);
    s3_only(&mut app);
    let effects = ctrl_s(&mut app);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app, 150, 50).contains("choose the backend for this run"));
    // Backend box: Tab to Repo, Base, Options, Backend; pick `work`; send.
    for _ in 0..4 {
        press(&mut app, KeyCode::Tab);
    }
    press(&mut app, KeyCode::Right);
    let effects = ctrl_s(&mut app);
    match effects.as_slice() {
        [Effect::SubmitDirectOn { backend, nonce, .. }] => {
            assert_eq!(backend, "work");
            assert!(nonce.starts_with("nonce-1"), "{nonce}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn b584_the_backend_from_the_command_line_or_the_default_is_the_pick_and_the_options_word_marks_customer_content()
 {
    let mut app = app(Some("spare"));
    s3_only(&mut app);
    // Options: mark the run as customer content; `spare` is not declared safe.
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab);
    }
    type_text(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(effects.is_empty(), "{effects:?}");
    let flat = screen(&app, 150, 50)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        flat.contains("not declared safe for customer content") && flat.contains("customerContent"),
        "{flat}"
    );
    // A pick that is no backend of the list is no pick.
    let mut app = self::app(Some("nope"));
    s3_only(&mut app);
    assert!(ctrl_s(&mut app).is_empty());
    assert!(screen(&app, 150, 50).contains("choose the backend"));
}

#[test]
fn b584_customer_content_on_a_backend_declared_safe_goes_through() {
    let mut app = app(Some("work"));
    s3_only(&mut app);
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab);
    }
    type_text(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SubmitDirectOn { backend, .. }] if backend == "work"),
        "{effects:?}"
    );
}
