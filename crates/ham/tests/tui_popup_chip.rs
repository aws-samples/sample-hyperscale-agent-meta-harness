//! The command popup and the plain-phrase chip of the message box.
//!
//! Popup: typing `/` in typing mode opens a list of the built commands above the box; each letter filters it; Up and Down move the marker;
//! Tab completes the marked entry; Enter on an entry without arguments sends `/word` through the router, and on an entry with arguments
//! completes `/word ` and sends nothing; Esc closes it. Nothing runs from the popup except through the router.
//!
//! Chip: a typed submission that is exactly one of the fixed phrases ("push my code", "pull the zip", "show my runs", "cancel this run")
//! is not sent: a chip asks "Run ham <command> for <run>? Enter = prepare it (preview and confirm follow), Esc = dismiss and keep the
//! draft". Enter prepares the command through the router (the preview and the second Enter follow); Esc dismisses the chip and keeps the
//! draft, and the same text is then sent as chat. A pasted, recalled or command-looking submission never gets a chip, and neither does
//! a bare word.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::mouse::Target;
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::registry;
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

struct Clock {
    base: Instant,
    ms: u64,
}

impl Clock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 0,
        }
    }

    fn after(&mut self, ms: u64) -> Instant {
        self.ms += ms;
        self.base + Duration::from_millis(self.ms)
    }
}

fn screen(app: &App) -> String {
    let (w, h) = (120, 40);
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
    out
}

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
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

/// The run on screen, in typing mode, ready for a calm typist.
fn ready() -> (App, Clock) {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(vec![
        env(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            None,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            3,
            EventType::GoalStatus,
            None,
            json!({"status": "active", "phase": "green"}),
        ),
    ]));
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    assert_eq!(app.mode(), Mode::Typing);
    // The 5 s after the `i` keep the first typed key out of its burst.
    clock.after(5_000);
    (app, clock)
}

fn press(app: &mut App, clock: &mut Clock, code: KeyCode, wait_ms: u64) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(KeyEvent::new(code, KeyModifiers::NONE), at)
}

fn type_text(app: &mut App, clock: &mut Clock, text: &str) {
    for c in text.chars() {
        let effects = press(app, clock, KeyCode::Char(c), 50);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(200);
}

fn enter(app: &mut App, clock: &mut Clock) -> Vec<Effect> {
    press(app, clock, KeyCode::Enter, 10)
}

/// The command words listed in the popup, top to bottom (rows that start with `/word` after the frame and the marker).
fn listed(text: &str) -> Vec<String> {
    let mut in_popup = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.contains("Commands") && line.contains('┌') {
            in_popup = true;
            continue;
        }
        if in_popup {
            if line.contains('└') {
                break;
            }
            let body = line
                .trim()
                .trim_start_matches('│')
                .trim()
                .trim_start_matches('▶')
                .trim();
            if let Some(rest) = body.strip_prefix('/') {
                let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
                out.push(word);
            }
        }
    }
    out
}

fn names(prefix: &str) -> Vec<String> {
    registry::popup(prefix)
        .into_iter()
        .map(|e| e.name.to_string())
        .collect()
}

// ---- the popup ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_slash_opens_the_popup_above_the_box() {
    let (mut app, mut clock) = ready();
    assert!(!screen(&app).contains("Commands"));
    type_text(&mut app, &mut clock, "/");
    let text = screen(&app);
    let listed = listed(&text);
    assert!(!listed.is_empty(), "{text}");
    assert_eq!(listed.first(), names("").first(), "{text}");
    // The popup sits between the conversation and the box.
    let pop = text.lines().position(|l| l.contains("Commands")).unwrap();
    let boxed = text
        .lines()
        .position(|l| l.contains("Message to Cline"))
        .unwrap();
    assert!(pop < boxed, "{text}");
}

#[test]
fn each_letter_filters_the_list_to_the_built_commands_with_that_prefix() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/pu");
    let shown = listed(&screen(&app));
    assert_eq!(shown, names("pu"), "{}", screen(&app));
    assert!(shown.contains(&"push".to_string()) && shown.contains(&"pull".to_string()));
    type_text(&mut app, &mut clock, "s");
    assert_eq!(listed(&screen(&app)), names("pus"));
    // A prefix nothing starts with closes the popup.
    type_text(&mut app, &mut clock, "zzz");
    assert!(!screen(&app).contains("Commands"));
}

#[test]
fn text_that_is_not_a_command_word_opens_nothing() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "hello");
    assert!(!screen(&app).contains("Commands"));
    let (mut app, mut clock) = ready();
    // A space ends the word: the arguments are not completed.
    type_text(&mut app, &mut clock, "/say hi");
    assert!(!screen(&app).contains("Commands"));
    // A leading space makes the line chat.
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, " /ru");
    assert!(!screen(&app).contains("Commands"));
    // In browse mode the box is not being typed in.
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/ru");
    press(&mut app, &mut clock, KeyCode::Esc, 10); // closes the popup
    press(&mut app, &mut clock, KeyCode::Esc, 10); // leaves typing
    assert_eq!(app.mode(), Mode::Browse);
    assert!(!screen(&app).contains("Commands"));
}

#[test]
fn up_and_down_move_the_marker_and_tab_completes_the_marked_entry() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/pu");
    let all = names("pu");
    assert!(all.len() >= 2, "{all:?}");
    let marked = |text: &str| -> String {
        text.lines()
            .find(|l| l.contains('▶') && l.contains(" /"))
            .map(|l| {
                let at = l.find('/').unwrap();
                l[at + 1..]
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(marked(&screen(&app)), all[0]);
    press(&mut app, &mut clock, KeyCode::Down, 50);
    assert_eq!(marked(&screen(&app)), all[1]);
    press(&mut app, &mut clock, KeyCode::Up, 50);
    press(&mut app, &mut clock, KeyCode::Up, 50);
    assert_eq!(marked(&screen(&app)), all[0], "the marker stops at the top");
    press(&mut app, &mut clock, KeyCode::Down, 50);
    // Tab writes the marked word into the box; an entry with arguments gets a blank after it.
    assert!(press(&mut app, &mut clock, KeyCode::Tab, 50).is_empty());
    let entry = registry::popup("pu")[1];
    let want = format!("/{}{}", entry.name, if entry.has_args() { " " } else { "" });
    assert_eq!(app.chat_text().as_deref(), Some(want.as_str()));
}

#[test]
fn enter_on_an_entry_with_arguments_completes_it_and_sends_nothing() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/can");
    assert_eq!(names("can"), ["cancel"]);
    assert!(registry::subcommand("cancel").unwrap().has_args());
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel "));
    assert_eq!(app.mode(), Mode::Typing);
    // The caret is ready for the arguments: the next character follows the blank.
    type_text(&mut app, &mut clock, "x");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel x"));
}

#[test]
fn enter_on_a_word_that_is_already_whole_sends_it_as_it_always_did() {
    // `/cancel` is a whole word: there is nothing left to complete, so Enter sends it and its preview follows (the run id is the focused run).
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/cancel");
    let effects = enter(&mut app, &mut clock);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("Confirm"), "{text}");
    assert!(text.contains("Press Enter to run it"), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel"));
}

#[test]
fn enter_on_an_entry_without_arguments_sends_it_through_the_router() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/doct");
    assert_eq!(names("doct"), ["doctor"]);
    assert!(!registry::subcommand("doctor").unwrap().has_args());
    let effects = enter(&mut app, &mut clock);
    match effects.as_slice() {
        [Effect::RunCommand { argv, .. }] => assert_eq!(argv, &["doctor".to_string()]),
        other => panic!("expected the router to run doctor: {other:?}"),
    }
}

#[test]
fn the_popup_never_runs_a_command_that_needs_its_second_enter() {
    // `/cancel` with no run id given takes the focused run, but a cancel waits for a confirm: Enter on the popup only completes.
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/cancel");
    let effects = enter(&mut app, &mut clock);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. })),
        "{effects:?}"
    );
}

#[test]
fn esc_closes_the_popup_and_a_new_letter_opens_it_again() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/ru");
    assert!(screen(&app).contains("Commands"));
    press(&mut app, &mut clock, KeyCode::Esc, 10);
    assert_eq!(
        app.mode(),
        Mode::Typing,
        "the first Esc only closes the popup"
    );
    assert!(!screen(&app).contains("Commands"));
    assert_eq!(app.chat_text().as_deref(), Some("/ru"));
    type_text(&mut app, &mut clock, "n");
    assert!(screen(&app).contains("Commands"));
    press(&mut app, &mut clock, KeyCode::Esc, 10);
    press(&mut app, &mut clock, KeyCode::Esc, 10);
    assert_eq!(app.mode(), Mode::Browse);
}

#[test]
fn the_popup_takes_the_click_itself() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "/pu");
    let _ = screen(&app);
    let map = app.hit_map();
    assert!(map.regions().iter().any(|r| r.target == Target::Block));
    assert!(map.unkeyed().is_empty());
}

#[test]
fn a_pasted_slash_word_opens_the_popup_but_stays_a_tainted_draft() {
    let (mut app, mut clock) = ready();
    app.handle_paste("/doctor".to_string());
    assert!(screen(&app).contains("Commands"));
    // A pasted draft always shows its preview before it runs: Enter does not run it at once.
    let effects = enter(&mut app, &mut clock);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. })),
        "{effects:?}"
    );
}

// ---- the chip -----------------------------------------------------------------------------------------------------------------------

fn chip_visible(text: &str) -> bool {
    text.contains(
        "Enter = prepare it (preview and confirm follow), Esc = dismiss and keep the draft",
    )
}

#[test]
fn a_listed_phrase_shows_a_chip_and_is_not_sent() {
    for (phrase, shell) in [
        ("push my code", "ham push"),
        ("push this run", "ham push"),
        ("pull the zip", "ham pull"),
        ("show my runs", "ham runs"),
        ("cancel this run", "ham cancel"),
    ] {
        let (mut app, mut clock) = ready();
        type_text(&mut app, &mut clock, phrase);
        let effects = enter(&mut app, &mut clock);
        assert!(effects.is_empty(), "{phrase}: {effects:?}");
        let text = screen(&app);
        assert!(chip_visible(&text), "{phrase}:\n{text}");
        assert!(text.contains(&format!("Run {shell}")), "{phrase}:\n{text}");
        // The draft is kept.
        assert_eq!(app.chat_text().as_deref(), Some(phrase));
    }
}

#[test]
fn the_chip_names_the_run_it_acts_on() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "cancel this run");
    enter(&mut app, &mut clock);
    let text = screen(&app);
    assert!(
        text.contains(&format!("Run ham cancel for {RUN}?")) || text.contains("for 01K6A"),
        "{text}"
    );
}

#[test]
fn matching_ignores_case_and_surrounding_blanks() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "  Show My RUNS  ");
    enter(&mut app, &mut clock);
    assert!(chip_visible(&screen(&app)));
}

#[test]
fn enter_on_the_chip_prepares_the_command_through_the_router() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "cancel this run");
    enter(&mut app, &mut clock);
    // The preview and the second Enter follow: the first Enter runs nothing.
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(!chip_visible(&text), "{text}");
    assert!(text.contains("Confirm"), "{text}");
    assert!(text.contains("Press Enter to run it"), "{text}");
    // The second Enter, after the delay, runs it.
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    match effects.as_slice() {
        [Effect::RunCommand { argv, .. }] => {
            assert_eq!(argv, &["cancel".to_string(), RUN.to_string()])
        }
        other => panic!("expected cancel to run: {other:?}"),
    }
}

#[test]
fn a_command_without_a_confirm_runs_at_once_after_the_chip() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "show my runs");
    enter(&mut app, &mut clock);
    let effects = press(&mut app, &mut clock, KeyCode::Enter, 400);
    match effects.as_slice() {
        [Effect::RunCommand { argv, .. }] => assert_eq!(argv, &["runs".to_string()]),
        other => panic!("expected runs to run: {other:?}"),
    }
}

#[test]
fn esc_dismisses_the_chip_keeps_the_draft_and_the_same_text_then_goes_as_chat() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "cancel this run");
    enter(&mut app, &mut clock);
    assert!(chip_visible(&screen(&app)));
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert!(effects.is_empty(), "Esc never sends: {effects:?}");
    assert_eq!(
        app.mode(),
        Mode::Typing,
        "the first Esc only dismisses the chip"
    );
    assert!(!chip_visible(&screen(&app)));
    assert_eq!(app.chat_text().as_deref(), Some("cancel this run"));
    // The same text is not offered again: Enter sends it as chat (no command runs). A calm pause first, so the Enter is not part of a burst.
    clock.after(300);
    let effects = enter(&mut app, &mut clock);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. })),
        "{effects:?}"
    );
    let text = screen(&app);
    assert!(!chip_visible(&text));
    // It was sent as chat, a message for after the turn: the box is emptied and a pending row shows it.
    let sent: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(ham::tui::ext::Job::SendMessage { request, .. }) => {
                Some(request.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["cancel this run"], "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(text.contains("cancel this run"), "{text}");
}

#[test]
fn another_edit_of_the_draft_drops_the_chip() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "push my code");
    enter(&mut app, &mut clock);
    assert!(chip_visible(&screen(&app)));
    type_text(&mut app, &mut clock, "!");
    assert!(!chip_visible(&screen(&app)));
    assert_eq!(app.chat_text().as_deref(), Some("push my code!"));
}

#[test]
fn a_bare_word_a_command_attempt_and_other_chat_get_no_chip() {
    for text in [
        "push",
        "cancel",
        "/push my code",
        "ham push my code",
        "please push my code now",
    ] {
        let (mut app, mut clock) = ready();
        type_text(&mut app, &mut clock, text);
        enter(&mut app, &mut clock);
        assert!(!chip_visible(&screen(&app)), "{text}");
    }
}

#[test]
fn a_pasted_phrase_gets_no_chip() {
    let (mut app, mut clock) = ready();
    app.handle_paste("cancel this run".to_string());
    let effects = enter(&mut app, &mut clock);
    assert!(!chip_visible(&screen(&app)));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. })),
        "{effects:?}"
    );
}

#[test]
fn the_chip_takes_the_click_itself() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "push my code");
    enter(&mut app, &mut clock);
    let _ = screen(&app);
    let map = app.hit_map();
    assert!(map.regions().iter().any(|r| r.target == Target::Block));
    assert!(map.unkeyed().is_empty());
}

// ---- engine text never offers a chip ----------------------------------------------------------------------------------------------
//
// "Only the user's submit event can offer the chip. Engine text cannot create a submit event." Text that Cline or the
// harness writes into the conversation is shown as it is; it does not reach the message box, does not press Enter for the person, and
// does not touch a chip that is waiting.

fn engine_text_events(first_id: u32, phrase: &str) -> Vec<Envelope> {
    let mut n = first_id;
    let mut next = || {
        n += 1;
        n
    };
    vec![
        env(
            next(),
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "message", "text": phrase}),
        ),
        env(
            next(),
            EventType::ToolSummary,
            Some("direct"),
            json!({"toolCallId": "t-chip", "title": phrase, "kind": "execute", "status": "pending"}),
        ),
        env(
            next(),
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "goal-loop", "phase": "on", "text": phrase}),
        ),
        env(
            next(),
            EventType::TurnProgress,
            Some("direct"),
            json!({
                "turn": 1, "kind": "needs-input", "text": phrase,
                "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1, "reason": "engine_stop", "title": phrase,
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "x", "runS": 412, "sessions": 1},
                "lastMessage": phrase, "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-14T20:30:00Z"
            }),
        ),
    ]
}

fn no_command_or_answer(effects: &[Effect]) -> bool {
    !effects
        .iter()
        .any(|e| matches!(e, Effect::RunCommand { .. } | Effect::Answer { .. }))
}

#[test]
fn engine_text_that_is_a_phrase_offers_no_chip_and_leaves_the_box_alone() {
    assert!(registry::PHRASES.len() >= 4);
    for (phrase, _) in registry::PHRASES {
        let (mut app, mut clock) = ready();
        let effects = app.update(pages(engine_text_events(100, phrase)));
        // TRIGGER PRESENT: the engine's words are on the screen, word for word.
        let text = screen(&app);
        assert!(
            text.to_lowercase().contains(phrase),
            "TRIGGER ABSENT {phrase}:\n{text}"
        );
        assert!(!chip_visible(&text), "{phrase}:\n{text}");
        assert!(no_command_or_answer(&effects), "{phrase}: {effects:?}");
        assert_eq!(
            app.chat_text().as_deref(),
            Some(""),
            "{phrase}: the box stays empty"
        );
        assert_eq!(app.mode(), Mode::Typing);
        // The person's own Enter on the empty box offers nothing either.
        let effects = enter(&mut app, &mut clock);
        assert!(effects.is_empty(), "{phrase}: {effects:?}");
        assert!(!chip_visible(&screen(&app)), "{phrase}");
    }
}

#[test]
fn engine_text_beside_a_typed_phrase_does_not_submit_it_and_the_persons_own_enter_still_does() {
    for (phrase, _) in registry::PHRASES {
        let (mut app, mut clock) = ready();
        type_text(&mut app, &mut clock, phrase);
        let effects = app.update(pages(engine_text_events(100, phrase)));
        assert!(no_command_or_answer(&effects), "{phrase}: {effects:?}");
        let text = screen(&app);
        assert!(
            !chip_visible(&text),
            "the engine's words are not a submit: {phrase}\n{text}"
        );
        assert_eq!(
            app.chat_text().as_deref(),
            Some(*phrase),
            "the draft is untouched"
        );
        // The person's own Enter is the only thing that offers the chip.
        let effects = enter(&mut app, &mut clock);
        assert!(effects.is_empty(), "{phrase}: {effects:?}");
        assert!(chip_visible(&screen(&app)), "{phrase}");
    }
}

#[test]
fn engine_text_while_a_chip_waits_neither_confirms_nor_dismisses_it() {
    let (mut app, mut clock) = ready();
    type_text(&mut app, &mut clock, "cancel this run");
    enter(&mut app, &mut clock);
    assert!(chip_visible(&screen(&app)));
    // Cline writes the same words, and a newer poll brings them in: the chip stays, nothing runs.
    let effects = app.update(pages(engine_text_events(100, "cancel this run")));
    assert!(no_command_or_answer(&effects), "{effects:?}");
    let text = screen(&app);
    assert!(chip_visible(&text), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("cancel this run"));
    // A tick (the screen refreshes on its own) changes nothing either.
    let effects = app.tick();
    assert!(no_command_or_answer(&effects), "{effects:?}");
    assert!(chip_visible(&screen(&app)));
    // Only the person's keys decide: Esc dismisses it.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert!(!chip_visible(&screen(&app)));
}

#[test]
fn browsing_a_block_of_engine_text_that_is_a_phrase_offers_no_chip() {
    for (phrase, _) in registry::PHRASES {
        let (mut app, mut clock) = ready();
        app.update(pages(engine_text_events(100, phrase)));
        // Leave the box and walk the blocks: Tab selects, Enter and Space act on the selected block.
        press(&mut app, &mut clock, KeyCode::Esc, 50);
        assert_eq!(app.mode(), Mode::Browse);
        for code in [
            KeyCode::Tab,
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ] {
            let effects = press(&mut app, &mut clock, code, 400);
            assert!(
                no_command_or_answer(&effects),
                "{phrase} {code:?}: {effects:?}"
            );
            let text = screen(&app);
            assert!(!chip_visible(&text), "{phrase} {code:?}:\n{text}");
        }
    }
}
