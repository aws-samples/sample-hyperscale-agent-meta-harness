//! The TUI half of the user correction path.
//! When agentd raises a question
//! (a `turn.progress` event of kind `needs-input`), the dashboard shows the card "Cline needs you", rings the terminal bell once,
//! and offers `p` (a message in the same Cline session), `s` (a fresh session on the same files), `m` (another model) and `f f`
//! (finish with what exists). A typing screen collects the text; `Ctrl-S` sends it. The outage notice is a card with no keys. The
//! words say what happened to the run and never claim the work failed or is done. Screens are pinned by insta snapshots at 80x24 and
//! 100x30; the keys, the effects and the messages are tested through `App`, which does no I/O.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, interactive_wait, perform};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::api::v1::{EventsPage, InputAccepted, InputAnswer};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::{InstructAction, banned_claim_word};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
/// The first question (an engine stop) and the second (the outage notice), as in the golden files of `fixtures/direct-input/`.
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const REQ2: &str = "01M5B8T4W2N6QJ3C7D9EH1FZ0A";
const OPENED: &str = "2026-10-15T13:41:07Z";
const EXPIRES: &str = "2026-10-15T14:11:07Z";
/// 27 minutes before the window closes.
const NOW: &str = "2026-10-15T13:44:07Z";
const SIZES: [(u16, u16); 2] = [(80, 24), (100, 30)];

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(key(c))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    press(app, KeyCode::Char(c))
}

fn type_text(app: &mut App, s: &str) {
    for c in s.chars() {
        let effects = if c == '\n' {
            press(app, KeyCode::Enter)
        } else {
            ch(app, c)
        };
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        // The version tag at the end of row 0 changes with every release (tui_version_tag.rs pins it); the snapshots hold the screen without it.
        out.push_str(if y == 0 {
            ham::tui::version_tag::split_tag(line.trim_end()).0
        } else {
            line.trim_end()
        });
        out.push('\n');
    }
    out
}

/// The text with its box-drawing characters dropped and its runs of white space squashed, so a sentence that wrapped over lines of a
/// panel can be found.
fn squashed(text: &str) -> String {
    text.replace(['│', '┌', '┐', '└', '┘', '─'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn snapshot_sizes(app: &App, name: &str) {
    for (w, h) in SIZES {
        insta::assert_snapshot!(format!("{name}_{w}x{h}"), screen_text(app, w, h));
    }
}

fn harness(n: u32, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: OPENED.into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

/// A `turn.progress` event of the direct agent; its id sorts after every question (`later`).
fn progress(event_id: String, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 0,
        ts: OPENED.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value: v },
    }
}

fn later(n: u32) -> String {
    format!("01M5C{n:021}")
}

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

/// The run while Cline works: created, one session, active.
fn working() -> Vec<Envelope> {
    vec![
        harness(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        harness(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": ham_core::models::DEFAULT_MODEL_ID}),
        ),
        harness(
            3,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

/// The payload of a blocking question, as agentd writes it; `over` replaces members.
fn question_payload(request_id: &str, n: u32, reason: &str, title: &str, over: Value) -> Value {
    let mut v = json!({
        "turn": 1,
        "kind": "needs-input",
        "text": format!("Needs your input: {title} (ham say, ham new-session or ham finish)"),
        "requestId": request_id,
        "n": n,
        "reason": reason,
        "title": title,
        "stopOutcome": "stopped_engine_cancelled",
        "evidence": {
            "attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
            "lastError": "the engine ended the prompt cancelled with no cancel request",
            "runS": 412, "sessions": 1
        },
        "lastMessage": "I will keep polling the build until it finishes, then append step 4 to PROGRESS.md.",
        "files": ["PROGRESS.md", "src/app.py", "tests/test_app.py"],
        "filesTotal": 3,
        "blocking": true,
        "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
        "models": [
            {"id": OPUS, "name": "Claude Opus 5.5", "contextTokens": 1000000},
            {"id": SONNET, "name": "Claude Sonnet 5.5", "contextTokens": 1000000}
        ],
        "recommended": null,
        "canContinue": true,
        "waitS": 1800,
        "expiresAt": EXPIRES
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn question(over: Value) -> Envelope {
    progress(
        REQ.into(),
        question_payload(REQ, 1, "engine_stop", "Cline stopped its own prompt", over),
    )
}

/// A question that offers no continue and no model: the engine process is gone.
fn question_without_continue() -> Envelope {
    progress(
        REQ.into(),
        question_payload(
            REQ,
            1,
            "engine_died",
            "The Cline process stopped",
            json!({"options": ["new_session_with_prompt", "finish"], "canContinue": false, "models": [], "stopOutcome": "stopped_engine_died"}),
        ),
    )
}

/// The non-blocking outage notice.
fn outage_notice() -> Envelope {
    progress(
        REQ2.into(),
        json!({
            "turn": 1, "kind": "needs-input",
            "text": "Every model route has been down for 30 minutes. The run keeps trying until its time limit (c c stops it).",
            "requestId": REQ2, "n": 2, "reason": "provider_outage", "title": "Every model route is down", "stopOutcome": null,
            "evidence": {"attempts": 14, "toolCalls": 212, "filesChanged": 9, "identicalCalls": null, "failedTurns": null, "lastError": "ThrottlingException", "runS": 9120, "sessions": 1},
            "lastMessage": null, "files": [], "filesTotal": 9, "blocking": false, "options": [], "models": [], "question": null,
            "recommended": null, "canContinue": false, "waitS": 0, "expiresAt": null
        }),
    )
}

fn received(n: u32, action: &str) -> Envelope {
    progress(
        later(n),
        json!({"turn": 1, "kind": "input-received", "text": "Your message reached Cline.", "requestId": REQ,
               "action": action, "origin": "user", "textBytes": 88, "waitedS": 93}),
    )
}

fn closed(n: u32, how: &str) -> Envelope {
    progress(
        later(n),
        json!({"turn": 1, "kind": "input-closed", "text": "closed", "requestId": REQ, "how": how, "waitedS": 1800}),
    )
}

fn new_session(n: u32) -> Envelope {
    progress(
        later(n),
        json!({"turn": 1, "kind": "new-session", "text": "Restarted from the work tree: fresh Cline session 2.",
               "requestId": REQ, "session": 2, "trigger": "user", "reason": "engine_stop", "restartedProcess": false,
               "digestBytes": 18432, "promptBytes": 21344}),
    )
}

/// An app on the run's dashboard whose clock reads 27 minutes before the window closes.
fn app_with(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(parse_utc_unix(NOW).unwrap());
    app.open_run(RUN);
    app.update(pages(events));
    app
}

fn waiting() -> App {
    let mut events = working();
    events.push(question(json!({})));
    app_with(events)
}

fn accepted() -> Msg {
    Msg::Answered(Ok(InputAccepted {
        run_id: RUN.into(),
        request_id: REQ.into(),
        state: "queued".into(),
    }))
}

fn answer(action: InstructAction, text: Option<&str>, model: Option<&str>) -> Effect {
    Effect::Answer {
        run_id: RUN.into(),
        answer: InputAnswer {
            request_id: REQ.into(),
            action,
            text: text.map(str::to_string),
            model: model.map(str::to_string),
        },
    }
}

// ---- the card and the bell -----------------------------------------------------------------------------------------------------

#[test]
fn a_question_shows_the_card_with_the_reason_the_numbers_the_last_words_and_the_keys() {
    let app = waiting();
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        let flat = squashed(&text);
        assert!(
            flat.contains("Cline needs you · about 27 min left to answer"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            flat.contains("Cline stopped its own prompt"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            flat.contains("41 tool calls, 3 files changed"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            flat.contains("Last words: I will keep polling the build"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            text.contains("p message · s fresh session · m other model · f f finish"),
            "{w}x{h}:\n{text}"
        );
        // The card states what happened to the run; it makes no claim about the work.
        assert_eq!(banned_claim_word(&text), None, "{w}x{h}:\n{text}");
        for absent in ["done", "success"] {
            assert!(
                !text.to_lowercase().contains(absent),
                "the card must not say {absent:?} at {w}x{h}:\n{text}"
            );
        }
    }
    snapshot_sizes(&app, "input_card_engine_stop");
}

#[test]
fn the_files_the_run_changed_are_listed_and_a_longer_list_says_how_many_more() {
    let mut events = working();
    events.push(question(json!({"filesTotal": 40})));
    let app = app_with(events);
    let flat = squashed(&screen_text(&app, 100, 30));
    assert!(
        flat.contains("Files: PROGRESS.md src/app.py tests/test_app.py (+37 more)"),
        "{flat}"
    );
}

#[test]
fn the_bell_rings_once_when_a_question_first_appears_and_not_again_while_it_stays_open() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(parse_utc_unix(NOW).unwrap());
    app.open_run(RUN);
    // The first load of a run that is already waiting rings.
    let mut events = working();
    events.push(question(json!({})));
    let first = app.update(pages(events.clone()));
    assert_eq!(
        first.iter().filter(|e| **e == Effect::Bell).count(),
        1,
        "{first:?}"
    );
    // The same events read again, and a later unrelated event, ring nothing.
    let again = app.update(pages(events.clone()));
    assert!(!again.contains(&Effect::Bell), "{again:?}");
    let more = app.update(pages(vec![progress(
        later(1),
        json!({"turn": 1, "kind": "provider-retry", "text": "waiting"}),
    )]));
    assert!(!more.contains(&Effect::Bell), "{more:?}");
    // The quiet timer rings nothing either.
    assert!(!app.tick().contains(&Effect::Bell));
    // A different question rings again.
    let second = app.update(pages(vec![outage_notice()]));
    assert_eq!(
        second.iter().filter(|e| **e == Effect::Bell).count(),
        1,
        "{second:?}"
    );
}

#[test]
fn a_run_with_no_question_has_no_card_no_bell_and_the_old_help_line() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let effects = app.update(pages(working()));
    assert!(!effects.contains(&Effect::Bell), "{effects:?}");
    let text = screen_text(&app, 80, 24);
    assert!(!text.contains("Cline needs you"), "{text}");
    assert!(
        text.contains(
            "↑↓ PgUp PgDn scroll · a artifacts · c cancel · Esc runs · n new run · q quit"
        ),
        "{text}"
    );
    // Without a question the answer keys do nothing and say so.
    for c in ['p', 's', 'm', 'f'] {
        assert!(ch(&mut app, c).is_empty());
        assert_eq!(app.screen(), Screen::Dashboard);
    }
}

#[test]
fn the_help_line_lists_the_answer_keys_while_a_question_is_open() {
    let app = waiting();
    let text = screen_text(&app, 80, 24);
    let help = text.lines().last().unwrap();
    assert!(
        help.contains("p message") && help.contains("f f finish") && help.contains("q quit"),
        "{help:?}"
    );
    assert!(help.chars().count() <= 80, "{help:?}");
}

#[test]
fn a_card_that_arrived_before_the_clock_is_set_falls_back_to_the_server_count_without_inventing_one()
 {
    // No `set_now`: the card still draws, and it does not show a made-up number of minutes.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut events = working();
    events.push(question(json!({})));
    app.update(pages(events));
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("Cline needs you"), "{text}");
    assert!(!text.contains("min left"), "{text}");
    assert!(text.contains("waiting for your answer"), "{text}");
}

#[test]
fn the_minutes_left_follow_the_clock_and_the_last_minute_and_a_closed_window_have_words() {
    let mut events = working();
    events.push(question(json!({})));
    let mut app = app_with(events);
    let line = |app: &App| {
        squashed(&screen_text(app, 100, 30))
            .split("Cline needs you")
            .nth(1)
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect::<String>()
    };
    app.set_now(parse_utc_unix("2026-10-15T13:50:07Z").unwrap());
    assert!(line(&app).contains("about 21 min left"), "{}", line(&app));
    app.set_now(parse_utc_unix("2026-10-15T14:10:30Z").unwrap());
    assert!(
        line(&app).contains("less than a minute left"),
        "{}",
        line(&app)
    );
    app.set_now(parse_utc_unix("2026-10-15T14:12:00Z").unwrap());
    assert!(
        line(&app).contains("no time left to answer"),
        "{}",
        line(&app)
    );
    app.set_now(parse_utc_unix("2026-10-15T11:00:07Z").unwrap());
    assert!(line(&app).contains("about 191 min left"), "{}", line(&app));
    app.set_now(parse_utc_unix("2026-10-15T14:10:07Z").unwrap());
    assert!(line(&app).contains("about 1 min left"), "{}", line(&app));
}

// ---- p: a message in the same session --------------------------------------------------------------------------------------------

#[test]
fn p_opens_the_typing_screen_and_ctrl_s_sends_the_text_in_the_same_session() {
    let mut app = waiting();
    assert!(ch(&mut app, 'p').is_empty());
    assert_eq!(app.screen(), Screen::Reply);
    type_text(
        &mut app,
        "Run the sleeps in the background\nand poll the log.",
    );
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("same session"), "{text}");
    assert!(text.contains("Run the sleeps in the background"), "{text}");
    assert!(text.contains("and poll the log."), "{text}");
    assert!(text.contains("Ctrl-S sends"), "{text}");
    snapshot_sizes(&app, "input_reply_continue");
    let effects = app.handle_key(ctrl_s());
    assert_eq!(
        effects,
        vec![answer(
            InstructAction::ContinueWithPrompt,
            Some("Run the sleeps in the background\nand poll the log."),
            None
        )]
    );
    // The answer is on its way: the screen says so and a second Ctrl-S sends nothing.
    assert!(screen_text(&app, 80, 24).contains("sending"));
    assert!(app.handle_key(ctrl_s()).is_empty());
}

#[test]
fn an_accepted_answer_returns_to_the_dashboard_with_a_notice_and_polls_at_once() {
    let mut app = waiting();
    ch(&mut app, 'p');
    type_text(&mut app, "go on");
    app.handle_key(ctrl_s());
    let effects = app.update(accepted());
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Poll { .. })),
        "a forced fast poll follows the answer: {effects:?}"
    );
    let text = screen_text(&app, 80, 24);
    // Queued is not applied: the notice says the message was sent, and the card says it is waiting for Cline to take it.
    assert!(text.contains("sent to the run"), "{text}");
    assert!(
        squashed(&text).contains("waiting for Cline to take it"),
        "{text}"
    );
    assert!(!text.contains("sending"), "{text}");
}

#[test]
fn a_message_needs_text() {
    let mut app = waiting();
    ch(&mut app, 'p');
    // Empty text: nothing is sent and the screen says why.
    assert!(app.handle_key(ctrl_s()).is_empty());
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("the message is empty"), "{text}");
    // Blank text counts as empty.
    type_text(&mut app, "  \n ");
    assert!(app.handle_key(ctrl_s()).is_empty());
    assert_eq!(app.screen(), Screen::Reply);
}

#[test]
fn text_over_262144_bytes_is_refused_with_the_count() {
    // The old limit was 8,000 characters. A person gets this much text into the box by pasting it, which is one edit.
    let mut app = waiting();
    ch(&mut app, 'p');
    assert!(app.handle_paste("x".repeat(8_001)).is_empty());
    assert_eq!(
        app.handle_key(ctrl_s()).len(),
        1,
        "8,001 characters are sent now"
    );
    let mut app = waiting();
    ch(&mut app, 'p');
    assert!(app.handle_paste("x".repeat(262_145)).is_empty());
    assert!(app.handle_key(ctrl_s()).is_empty());
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("262,145"), "{text}");
    assert!(text.contains("262,144"), "{text}");
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.handle_key(ctrl_s()).len(), 1);
    // The limit counts bytes: 65,536 four-byte characters are 262,144 bytes and pass.
    let mut app = waiting();
    ch(&mut app, 'p');
    assert!(app.handle_paste("\u{1F600}".repeat(65_536)).is_empty());
    assert_eq!(app.handle_key(ctrl_s()).len(), 1);
}

#[test]
fn esc_returns_to_the_dashboard_and_keeps_the_question_open_and_the_text() {
    let mut app = waiting();
    ch(&mut app, 'p');
    type_text(&mut app, "half a thought");
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("Cline needs you"), "{text}");
    // Pressing p again brings the text back.
    ch(&mut app, 'p');
    assert!(screen_text(&app, 80, 24).contains("half a thought"));
    assert_eq!(
        app.handle_key(ctrl_s()),
        vec![answer(
            InstructAction::ContinueWithPrompt,
            Some("half a thought"),
            None
        )]
    );
}

#[test]
fn p_and_m_are_refused_with_one_line_when_cline_cannot_continue_and_s_still_works() {
    let mut events = working();
    events.push(question_without_continue());
    let mut app = app_with(events);
    let text = screen_text(&app, 80, 24);
    assert!(
        text.contains("s fresh session") && text.contains("f f finish"),
        "{text}"
    );
    assert!(
        !text.contains("p message") && !text.contains("m other model"),
        "{text}"
    );
    assert!(ch(&mut app, 'p').is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    let text = screen_text(&app, 80, 24);
    assert!(
        squashed(&text).contains("cannot take a message"),
        "the refusal is one line on the dashboard:\n{text}"
    );
    assert!(ch(&mut app, 'm').is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    snapshot_sizes(&app, "input_card_no_continue");
    ch(&mut app, 's');
    assert_eq!(app.screen(), Screen::Reply);
}

// ---- s: a fresh session ----------------------------------------------------------------------------------------------------------

#[test]
fn s_starts_a_fresh_session_and_the_text_is_optional() {
    let mut app = waiting();
    ch(&mut app, 's');
    assert_eq!(app.screen(), Screen::Reply);
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("fresh session"), "{text}");
    assert!(text.contains("optional"), "{text}");
    snapshot_sizes(&app, "input_reply_fresh_empty");
    assert_eq!(
        app.handle_key(ctrl_s()),
        vec![answer(InstructAction::NewSessionWithPrompt, None, None)]
    );
}

#[test]
fn s_with_text_sends_the_text_with_the_fresh_session() {
    let mut app = waiting();
    ch(&mut app, 's');
    type_text(&mut app, "start from the failing test");
    assert_eq!(
        app.handle_key(ctrl_s()),
        vec![answer(
            InstructAction::NewSessionWithPrompt,
            Some("start from the failing test"),
            None
        )]
    );
}

// ---- m: another model ------------------------------------------------------------------------------------------------------------

#[test]
fn m_lists_the_models_of_the_question_and_the_choice_goes_to_the_typing_screen() {
    let mut app = waiting();
    assert!(ch(&mut app, 'm').is_empty());
    assert_eq!(app.screen(), Screen::Models);
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("Claude Opus 5.5"), "{text}");
    assert!(text.contains("Claude Sonnet 5.5"), "{text}");
    snapshot_sizes(&app, "input_model_list");
    // Down, Enter picks the second model; the typing screen names it and takes optional text.
    assert!(press(&mut app, KeyCode::Down).is_empty());
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(app.screen(), Screen::Reply);
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("Claude Sonnet 5.5"), "{text}");
    assert!(text.contains("optional"), "{text}");
    assert_eq!(
        app.handle_key(ctrl_s()),
        vec![answer(InstructAction::TryModel, None, Some(SONNET))]
    );
}

#[test]
fn the_model_list_stops_at_its_ends_and_esc_goes_back_to_the_card() {
    let mut app = waiting();
    ch(&mut app, 'm');
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "use the other one");
    assert_eq!(
        app.handle_key(ctrl_s()),
        vec![answer(
            InstructAction::TryModel,
            Some("use the other one"),
            Some(SONNET)
        )]
    );
    let mut app = waiting();
    ch(&mut app, 'm');
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(screen_text(&app, 80, 24).contains("Cline needs you"));
}

#[test]
fn m_says_so_when_no_other_model_is_offered() {
    let mut events = working();
    events.push(question(json!({"options": ["continue_with_prompt", "new_session_with_prompt", "finish"], "models": []})));
    let mut app = app_with(events);
    assert!(!screen_text(&app, 80, 24).contains("m other model"));
    assert!(ch(&mut app, 'm').is_empty());
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(squashed(&screen_text(&app, 80, 24)).contains("no other model"));
}

// ---- f f: finish -----------------------------------------------------------------------------------------------------------------

#[test]
fn f_twice_finishes_the_run_with_what_exists() {
    let mut app = waiting();
    assert!(ch(&mut app, 'f').is_empty());
    let text = screen_text(&app, 80, 24);
    assert!(
        squashed(&text).contains("press f again to finish"),
        "{text}"
    );
    assert_eq!(
        ch(&mut app, 'f'),
        vec![answer(InstructAction::Finish, None, None)]
    );
}

#[test]
fn any_other_key_between_the_two_f_presses_cancels_the_finish() {
    let mut app = waiting();
    ch(&mut app, 'f');
    assert!(ch(&mut app, 'x').is_empty());
    assert!(
        ch(&mut app, 'f').is_empty(),
        "the first f of a new pair only asks"
    );
    // c c and f f do not mix.
    let mut app = waiting();
    ch(&mut app, 'c');
    assert!(ch(&mut app, 'f').is_empty());
    assert!(ch(&mut app, 'f').len() == 1);
}

// ---- the answer's result ---------------------------------------------------------------------------------------------------------

#[test]
fn a_refusal_from_the_server_shows_its_plain_message_and_keeps_the_text() {
    let mut app = waiting();
    ch(&mut app, 'p');
    type_text(&mut app, "too late?");
    app.handle_key(ctrl_s());
    let msg = "this question was already answered with finish";
    let effects = app.update(Msg::Answered(Err(msg.into())));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Poll { .. })),
        "a refusal is followed by a poll that shows the real state: {effects:?}"
    );
    let text = screen_text(&app, 100, 30);
    assert!(squashed(&text).contains(msg), "{text}");
    assert!(text.contains("too late?"), "the text is not lost:\n{text}");
    // Esc leaves; the dashboard's notice line keeps the message.
    press(&mut app, KeyCode::Esc);
    assert!(squashed(&screen_text(&app, 100, 30)).contains(msg));
    // A window that closed (410) reads the same way.
    let mut app = waiting();
    ch(&mut app, 'f');
    ch(&mut app, 'f');
    let gone = "the answer window closed at 2026-10-15T14:11:07Z";
    app.update(Msg::Answered(Err(gone.into())));
    assert!(squashed(&screen_text(&app, 100, 30)).contains(gone));
}

#[test]
fn a_question_that_closes_while_the_user_types_says_so_and_keeps_the_text() {
    let mut app = waiting();
    ch(&mut app, 'p');
    type_text(&mut app, "my long message");
    app.update(pages(vec![closed(1, "expired")]));
    assert_eq!(app.screen(), Screen::Reply);
    let text = screen_text(&app, 80, 24);
    assert!(
        squashed(&text).contains("this question is closed"),
        "{text}"
    );
    assert!(text.contains("my long message"), "{text}");
    assert!(
        app.handle_key(ctrl_s()).is_empty(),
        "nothing is sent to a closed question"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Dashboard);
}

// ---- the end of a question -------------------------------------------------------------------------------------------------------

#[test]
fn when_the_question_is_answered_the_card_goes_and_the_notice_keeps_the_outcome() {
    let mut app = waiting();
    app.update(pages(vec![
        received(1, "new_session_with_prompt"),
        new_session(2),
    ]));
    let text = screen_text(&app, 80, 24);
    assert!(!text.contains("Cline needs you"), "{text}");
    assert!(text.contains("answered: fresh session"), "{text}");
    assert!(
        text.contains("Session 2 · restarted from the work tree"),
        "{text}"
    );
    // The same for the other actions.
    for (action, words) in [
        ("continue_with_prompt", "answered: message sent to Cline"),
        ("try_model", "answered: other model"),
    ] {
        let mut app = waiting();
        app.update(pages(vec![received(1, action)]));
        let text = screen_text(&app, 100, 30);
        assert!(text.contains(words), "{action}:\n{text}");
    }
    let mut app = waiting();
    app.update(pages(vec![received(1, "finish"), closed(2, "finish")]));
    assert!(screen_text(&app, 100, 30).contains("answered: finish"));
}

#[test]
fn when_nobody_answers_the_notice_says_the_window_closed() {
    let mut app = waiting();
    app.update(pages(vec![closed(1, "expired")]));
    let text = screen_text(&app, 80, 24);
    assert!(!text.contains("Cline needs you"), "{text}");
    assert!(text.contains("window closed: nobody answered"), "{text}");
}

#[test]
fn a_new_question_replaces_the_old_one_and_resets_the_finish_confirmation() {
    let mut app = waiting();
    ch(&mut app, 'f');
    app.update(pages(vec![progress(
        REQ2.into(),
        question_payload(
            REQ2,
            2,
            "no_changes",
            "Cline ended without changing files",
            json!({}),
        ),
    )]));
    // The first f belonged to the old question: the next f only asks again.
    assert!(ch(&mut app, 'f').is_empty());
}

// ---- the outage notice -----------------------------------------------------------------------------------------------------------

#[test]
fn the_outage_notice_has_the_card_without_keys() {
    let mut events = working();
    events.push(outage_notice());
    let mut app = app_with(events);
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        let flat = squashed(&text);
        assert!(
            flat.contains("Every model route is down"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            flat.contains(
                "The run keeps probing until its time limit. c c stops it and delivers what exists."
            ),
            "{w}x{h}:\n{text}"
        );
        assert!(!text.contains("p message"), "{w}x{h}:\n{text}");
        assert!(!text.contains("f f finish"), "{w}x{h}:\n{text}");
        assert!(!text.contains("to answer"), "{w}x{h}:\n{text}");
    }
    snapshot_sizes(&app, "input_outage_notice");
    // The answer keys do nothing for a notice.
    for c in ['p', 's', 'm', 'f'] {
        assert!(ch(&mut app, c).is_empty());
        assert_eq!(app.screen(), Screen::Dashboard);
    }
    // A notice rings the bell on its first read like any new condition (see the bell test), and goes when a route answers.
    app.update(pages(vec![progress(
        later(1),
        json!({"turn": 1, "kind": "input-closed", "text": "recovered", "requestId": REQ2, "how": "recovered", "waitedS": 0}),
    )]));
    // The card goes; the conversation keeps the line of what happened, as a chat keeps what was said.
    let text = screen_text(&app, 80, 24);
    assert!(
        !text.contains("The run keeps probing until its time limit"),
        "{text}"
    );
}

// ---- other screens ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_runs_screen_marks_a_run_that_needs_input() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let runs: Vec<harness_protocol::api::v1::RunSummary> = serde_json::from_value(json!([
        {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
         "createdAt": "2026-10-15T13:00:00Z", "ownerPrincipalArn": "arn:aws:iam::700000000011:role/test",
         "needsInput": {"requestId": REQ, "reason": "engine_stop", "title": "Cline stopped its own prompt", "expiresAt": EXPIRES}},
        {"runId": "01K6A0000000000000000000R0", "status": "active", "phase": "green", "target": {"mode": "s3-only"},
         "createdAt": "2026-10-15T12:00:00Z", "ownerPrincipalArn": "arn:aws:iam::700000000011:role/test"}
    ]))
    .unwrap();
    app.update(Msg::Runs(Ok(runs)));
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("needs input"), "{text}");
    assert!(
        squashed(&text).contains("needs input: Cline stopped its own prompt"),
        "the note line names the reason of the selected run:\n{text}"
    );
    assert!(squashed(&text).contains("1 needs input"), "{text}");
    snapshot_sizes(&app, "input_runs_marked");
}

#[test]
fn a_fresh_session_adds_one_line_to_the_status_block() {
    let mut events = working();
    events.push(new_session(1));
    let app = app_with(events);
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        assert!(
            text.contains("Session 2 · restarted from the work tree"),
            "{w}x{h}:\n{text}"
        );
    }
    // A run with one session draws no such line.
    let app = app_with(working());
    assert!(!screen_text(&app, 80, 24).contains("restarted from the work tree"));
}

#[test]
fn text_from_the_run_cannot_put_escape_sequences_on_the_screen() {
    let mut events = working();
    events.push(question(json!({
        "title": "Cline stopped\u{1b}[31m its own prompt",
        "lastMessage": "red \u{1b}[31mtext\u{1b}[0m\nsecond \u{7} line",
        "files": ["a\u{1b}[2Jb.py"]
    })));
    let app = app_with(events);
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
    }
}

#[test]
fn no_screen_panics_on_a_tiny_terminal() {
    let mut app = waiting();
    for (w, h) in [(20, 6), (40, 10), (60, 14), (80, 8), (120, 5)] {
        let _ = screen_text(&app, w, h);
    }
    ch(&mut app, 'm');
    for (w, h) in [(20, 6), (40, 10), (80, 8)] {
        let _ = screen_text(&app, w, h);
    }
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "x\ny\nz");
    for (w, h) in [(20, 6), (40, 10), (80, 8)] {
        let _ = screen_text(&app, w, h);
    }
}

#[test]
fn the_dashboard_keeps_its_agent_list_while_the_card_is_open() {
    // The agent list is a folded card of the conversation: its title shows while the question is open, and Space on it opens the table.
    let mut app = waiting();
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("agents 1"), "{text}");
    assert!(!text.contains("AGENT"), "{text}");
    // Shift-Tab selects the last block (the question), a second one the agents before it.
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Char(' '));
    let text = screen_text(&app, 120, 40);
    assert!(text.contains("AGENT"), "{text}");
    assert!(text.contains("direct"), "{text}");
}

#[test]
fn the_card_never_draws_more_rows_than_the_screen_has() {
    // A long last message and a long explanation at the smallest supported size still leave the agent list and the help line.
    let long = "word ".repeat(400);
    let mut events = working();
    events.push(question(json!({"lastMessage": long})));
    let app = app_with(events);
    let text = screen_text(&app, 80, 24);
    // The card is a block of the conversation, which scrolls: the screen keeps the box, the footer and the keys line that ends the card.
    assert!(text.contains("Message to Cline"), "{text}");
    assert!(text.contains("q quit"), "{text}");
    assert!(text.contains("f f finish"), "{text}");
    assert!(text.contains("p message \u{b7} s fresh session"), "{text}");
    assert!(text.lines().count() <= 24, "{text}");
}

// ---- the window the composer asks for --------------------------------------------------------------------------------------------

#[test]
fn the_composer_asks_for_a_thirty_minute_window_unless_the_user_chose_another() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    press(&mut app, KeyCode::Char('n'));
    type_text(&mut app, "Add a percent_change function");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right); // --s3-only
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter); // confirms the model on the screen
    let effects = app.handle_key(ctrl_s());
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let none = |_: &str| None::<String>;
    let cfg = ham_core::config::Config::default();
    // Nothing set: an interactive client asks for 30 minutes.
    assert_eq!(
        interactive_wait(args, &none, &cfg).unwrap().input_wait,
        Some(30)
    );
    // HAMH_INPUT_WAIT beats the default, the config file sits between, and 0 is the user's way back to today's behaviour.
    let env = |k: &str| (k == "HAMH_INPUT_WAIT").then(|| "10".to_string());
    assert_eq!(
        interactive_wait(args, &env, &cfg).unwrap().input_wait,
        Some(10)
    );
    let cfg0 = ham_core::config::Config {
        input_wait_minutes: Some(0),
        ..ham_core::config::Config::default()
    };
    assert_eq!(
        interactive_wait(args, &none, &cfg0).unwrap().input_wait,
        Some(0)
    );
    // A value that is set and wrong is an error that names its layer; it never falls back to the default.
    let bad = |k: &str| (k == "HAMH_INPUT_WAIT").then(|| "soon".to_string());
    let e = interactive_wait(args, &bad, &cfg).unwrap_err();
    assert!(e.contains("HAMH_INPUT_WAIT"), "{e}");
}

// ---- through the real event loop and the control plane --------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

/// A run on the fake plane whose Cline asked a question; agentd (the fake) takes every answer at once.
async fn asked_run(label: &str) -> (Arc<Fake>, Api<HttpsPlane>, String, String) {
    let fake = Fake::start().await;
    fake.mem.inner.lock().unwrap().auto_ack_input = true;
    let a = api(&fake);
    let run = a
        .start_direct_run(
            &serde_json::from_value::<StartDirectRunRequest>(
                json!({"prompt": label, "target": {"mode": "s3-only"}, "model": ham_core::models::DEFAULT_MODEL_ID}),
            )
            .unwrap(),
            label,
        )
        .await
        .unwrap()
        .run_id;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let q = fake.mem.needs_input(&run, "engine_stop");
    (fake, a, run, q)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_answer_effect_posts_to_the_control_plane_and_a_stale_answer_comes_back_in_plain_words()
{
    let (fake, a, run, q) = asked_run("tui-answer").await;
    let dir = support::scratch("tui-answer");
    let answer = |request_id: &str, text: &str| Effect::Answer {
        run_id: run.clone(),
        answer: InputAnswer {
            request_id: request_id.into(),
            action: InstructAction::ContinueWithPrompt,
            text: Some(text.into()),
            model: None,
        },
    };
    let Some(Msg::Answered(Ok(accepted))) = perform(&a, &dir, answer(&q, "go on")).await else {
        panic!("the answer was not queued");
    };
    assert_eq!(
        (accepted.run_id.as_str(), accepted.request_id.as_str()),
        (run.as_str(), q.as_str())
    );
    let queued = fake.mem.controls_of(&run);
    assert_eq!(queued.len(), 1, "{queued:?}");
    // The fake agentd took it: the question is closed, so a second answer to it is refused in the server's words.
    let Some(Msg::Answered(Err(why))) = perform(&a, &dir, answer(&q, "another message")).await
    else {
        panic!("a stale answer must be refused");
    };
    assert!(!why.is_empty() && !why.contains('\u{1b}'), "{why}");
    assert_eq!(
        fake.mem.controls_of(&run).len(),
        1,
        "nothing more was queued"
    );
    // The bell writes to the terminal and yields no message.
    assert!(perform(&a, &dir, Effect::Bell).await.is_none());
}

/// Drives `ham::tui::event_loop` the way a person does: the card is up, press p, type, send, and the question closes. The plane answers
/// the run's event reads `first_poll_delay_ms` late until the driver has seen the first poll applied.
async fn answer_through_the_event_loop(label: &str, first_poll_delay_ms: u64) {
    let (fake, a, run, q) = asked_run(label).await;
    fake.faults
        .events_delay_ms
        .store(first_poll_delay_ms, std::sync::atomic::Ordering::SeqCst);
    let dir = support::scratch(label);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let first = app.open_run(&run);
    let driver = tokio::spawn({
        let fake = fake.clone();
        let run = run.clone();
        async move {
            let wait_for = |what: &'static str, mut cond: Box<dyn FnMut() -> bool + Send>| async move {
                for _ in 0..600 {
                    if cond() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                panic!("timed out waiting for {what}");
            };
            // The card needs the first poll applied, not only served. A poll reads every shard once (EVENT_SHARDS requests, which arrive
            // together) and the next poll starts only after the app has taken the first one's pages (`Msg::Pages` clears `polling`). So
            // once the plane has served more than EVENT_SHARDS reads of the run's events, the question is known and p opens the typing
            // screen. Waiting for two reads let p come before the pages did: the app had no question, ignored the keys, the answer never
            // came and the test hung (about 7 percent of runs).
            let (f1, r1) = (fake.clone(), run.clone());
            wait_for(
                "a second poll of the run's events (the first one applied)",
                Box::new(move || {
                    f1.mem
                        .seen()
                        .iter()
                        .filter(|r| r.method == "GET" && r.path.contains(&format!("{r1}/events")))
                        .count()
                        > usize::from(harness_protocol::store::EVENT_SHARDS)
                }),
            )
            .await;
            fake.faults
                .events_delay_ms
                .store(0, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let key =
                |c: KeyCode| crossterm::event::Event::Key(KeyEvent::new(c, KeyModifiers::NONE));
            tx.send(key(KeyCode::Char('p'))).unwrap();
            for c in "go on".chars() {
                tx.send(key(KeyCode::Char(c))).unwrap();
            }
            tx.send(crossterm::event::Event::Key(ctrl_s())).unwrap();
            let f2 = fake.clone();
            let r2 = run.clone();
            wait_for(
                "the answer to reach the plane",
                Box::new(move || !f2.mem.controls_of(&r2).is_empty()),
            )
            .await;
            // Give the loop time to poll the acknowledgement, then quit.
            tokio::time::sleep(Duration::from_millis(600)).await;
            tx.send(crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
            tx
        }
    });
    // A loop that does not end is a failure, not a stalled test run.
    let view = tokio::time::timeout(
        Duration::from_secs(20),
        ham::tui::event_loop(
            &mut term,
            a.clone(),
            dir,
            app,
            first,
            rx,
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("the event loop did not end")
    .unwrap()
    .expect("the focused run's view");
    let _tx = driver.await.unwrap();
    assert!(
        view.input.open.is_none(),
        "Cline took the answer, so the question is closed: {:?}",
        view.input
    );
    let outcome = view
        .input
        .history
        .iter()
        .find(|h| h.request_id == q)
        .expect("the question is in the history");
    assert_eq!(outcome.how, "answered:continue_with_prompt");
    let sent = fake.mem.controls_of(&run);
    assert_eq!(sent.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tui_notices_a_question_answers_it_and_sees_it_close_through_the_real_event_loop() {
    answer_through_the_event_loop("tui-loop-answer", 0).await;
}

/// The same path when the plane is slow to serve the first poll: the keys must wait until the card is up, however long that takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tui_answers_through_the_real_event_loop_when_the_first_poll_is_slow() {
    answer_through_the_event_loop("tui-loop-slow", 400).await;
}
