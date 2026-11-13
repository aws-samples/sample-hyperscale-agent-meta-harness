//! The chat box runs commands: the line sent from the message box goes through the router of
//! `ham-core` (`route`), and what comes back is acted on here. Chat goes to the open question as before; a registry word that reads
//! only runs at once and its text appears in a result panel; a command that changes a run, sends data or changes a security setting
//! shows a preview and waits for a second Enter (not sooner than 300 ms after the preview, never inside a burst or right after a
//! paste, with the characters asked for typed first when the preview asks for them); text that was pasted never runs without that
//! preview; a word that does not exist is refused with the closest names; `/new`, `/open`, `/watch`, `/help` and a start form
//! typed as `ham "..." --direct` are handled by the screen itself; commands that need the terminal (an editor, git's prompts) show their preview
//! and, after the second Enter, are handed the terminal as a child `ham`. Nothing else here sends a request by
//! itself: every other command is an `Effect::RunCommand`.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::output::{Chunk, CmdOutput, Stream};
use ham::sigv4::Signer;
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use ham_core::command::parse;
use harness_protocol::api::v1::{EventsPage, InputAnswer};
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
const RUN2: &str = "01K6A0000000000000000000R2";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const OPENED: &str = "2026-10-15T13:41:07Z";
const EXPIRES: &str = "2026-10-15T14:11:07Z";
const LINK: &str = "https://downloads.example.com/secret-link-9f3a/run.zip?sig=abc123";

/// Keys with their own clock, so the 30 ms burst gap and the 300 ms preview delay are exercised without sleeping.
struct Clock {
    base: Instant,
    ms: u64,
}

impl Clock {
    /// A clock for a test: it starts 5 s after the clock that opened the run screen (`ready`), so the first key of a test is not part of
    /// a burst with the `i` that started typing.
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 5_000,
        }
    }

    /// The clock `ready` uses.
    fn early() -> Self {
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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(app: &mut App, clock: &mut Clock, code: KeyCode, wait_ms: u64) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(key(code), at)
}

/// Types `text` at a calm pace (one key every 50 ms), then waits 200 ms.
fn type_text(app: &mut App, clock: &mut Clock, text: &str) {
    for c in text.chars() {
        let at = clock.after(50);
        let effects = app.handle_key_at(key(KeyCode::Char(c)), at);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
    clock.after(200);
}

fn enter(app: &mut App, clock: &mut Clock, wait_ms: u64) -> Vec<Effect> {
    press(app, clock, KeyCode::Enter, wait_ms)
}

fn screen(app: &App, w: u16, h: u16) -> String {
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

fn harness(seq: u64, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq,
        ts: OPENED.into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn question() -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: REQ.into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 0,
        ts: OPENED.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({
                "turn": 1, "kind": "needs-input",
                "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
                "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "x", "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": EXPIRES
            }),
        },
    }
}

fn pages(app: &mut App, with_question: bool) {
    let mut events = vec![
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
    ];
    if with_question {
        events.push(question());
    }
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    app.update(Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect())));
}

/// The run `RUN` on screen in typing mode, with its events loaded.
fn ready(with_question: bool) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    pages(&mut app, with_question);
    let mut clock = Clock::early();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    assert_eq!(app.mode(), Mode::Typing);
    app
}

/// Types `line` and sends it with Enter; returns the effects of the Enter.
fn send(app: &mut App, clock: &mut Clock, line: &str) -> Vec<Effect> {
    type_text(app, clock, line);
    enter(app, clock, 10)
}

fn out(text: &str, code: i32, bearer: Option<&str>) -> CmdOutput {
    CmdOutput {
        chunks: vec![Chunk {
            stream: Stream::Out,
            text: text.into(),
        }],
        code,
        bearer: bearer.map(str::to_string),
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

fn run_command(effects: &[Effect]) -> Vec<String> {
    match effects {
        [Effect::RunCommand { argv, invocation }] => {
            assert_eq!(
                &parse(argv).unwrap(),
                invocation,
                "the invocation is what the parser reads from the argv"
            );
            argv.clone()
        }
        other => panic!("expected one RunCommand, got {other:?}"),
    }
}

// ---- chat stays chat -----------------------------------------------------------------------------------------------------------

#[test]
fn a_plain_line_still_answers_the_open_question() {
    let mut app = ready(true);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "use the other API");
    let [Effect::Answer { run_id, answer }] = effects.as_slice() else {
        panic!("expected one answer, got {effects:?}");
    };
    assert_eq!(run_id, RUN);
    assert_eq!(
        answer,
        &InputAnswer {
            request_id: REQ.into(),
            action: InstructAction::ContinueWithPrompt,
            text: Some("use the other API".into()),
            model: None,
        }
    );
}

#[test]
fn slash_send_sends_the_rest_as_chat_exactly_as_typed() {
    let mut app = ready(true);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/send /cancel  it all");
    let [Effect::Answer { answer, .. }] = effects.as_slice() else {
        panic!("expected one answer, got {effects:?}");
    };
    assert_eq!(answer.text.as_deref(), Some("/cancel  it all"));
}

#[test]
fn a_line_with_a_leading_space_or_a_capital_is_chat() {
    for line in ["/cancel", " /cancel", "Ham push", "ham"] {
        // Only the first of these is a command; the others reach the question.
        let mut app = ready(true);
        let mut clock = Clock::new();
        let effects = send(&mut app, &mut clock, line);
        if line == "/cancel" {
            assert!(effects.is_empty(), "{line:?} ran at once: {effects:?}");
        } else {
            assert!(
                matches!(effects.as_slice(), [Effect::Answer { .. }]),
                "{line:?} was not chat: {effects:?}"
            );
        }
    }
}

// ---- refusals ------------------------------------------------------------------------------------------------------------------

#[test]
fn a_word_that_does_not_exist_is_refused_with_the_closest_names_and_the_text_stays() {
    let mut app = ready(true);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/cancle");
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("/cancle"));
    let text = screen(&app, 100, 30);
    assert!(text.contains("no command"), "{text}");
    assert!(text.contains("cancel"), "{text}");
}

#[test]
fn two_lines_are_chat_or_nothing_never_a_command() {
    let mut app = ready(true);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/cancel");
    app.handle_key_at(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        clock.after(50),
    );
    type_text(&mut app, &mut clock, "and the rest");
    let effects = enter(&mut app, &mut clock, 10);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 100, 30);
    assert!(text.contains("one line"), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel\nand the rest"));
}

// ---- commands that run at once -------------------------------------------------------------------------------------------------

#[test]
fn a_command_that_only_reads_runs_at_once_and_the_box_empties() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/runs");
    assert_eq!(run_command(&effects), argv(&["runs"]));
    assert_eq!(app.chat_text().as_deref(), Some(""));
    let text = screen(&app, 100, 30);
    assert!(
        text.contains("ham runs"),
        "the panel names the command:\n{text}"
    );
}

#[test]
fn the_shell_form_runs_the_same_command() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "ham models");
    assert_eq!(run_command(&effects), argv(&["models"]));
}

#[test]
fn a_command_without_a_run_id_acts_on_the_focused_run() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/link");
    assert_eq!(run_command(&effects), argv(&["link", RUN]));
}

#[test]
fn a_local_write_runs_at_once() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/pull");
    assert_eq!(run_command(&effects), argv(&["pull", RUN]));
}

// ---- the second Enter ------------------------------------------------------------------------------------------------------------

#[test]
fn a_command_that_changes_a_run_shows_a_preview_and_waits_for_a_second_enter() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/cancel");
    assert!(
        effects.is_empty(),
        "ran without a second Enter: {effects:?}"
    );
    let text = screen(&app, 100, 30);
    assert!(text.contains(&format!("Cancel run {RUN}")), "{text}");
    assert!(text.contains("Enter"), "the preview names its key:\n{text}");
    // The text stays in the box while the preview is up.
    assert_eq!(app.chat_text().as_deref(), Some("/cancel"));
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["cancel", RUN]));
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(
        !screen(&app, 100, 30).contains("Cancel run"),
        "the preview stays after it ran"
    );
}

#[test]
fn an_enter_less_than_300_ms_after_the_preview_does_nothing() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let effects = enter(&mut app, &mut clock, 100);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app, 100, 30).contains("Cancel run"));
    // 150 ms later it is past 300 ms from the preview: the next Enter runs it.
    let effects = enter(&mut app, &mut clock, 250);
    assert_eq!(run_command(&effects), argv(&["cancel", RUN]));
}

#[test]
fn esc_cancels_the_preview_and_keeps_the_text() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(effects.is_empty());
    assert_eq!(app.mode(), Mode::Typing, "Esc cancelled the preview only");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel"));
    assert!(!screen(&app, 100, 30).contains("Cancel run"));
    // The Enter that follows previews again; nothing ran.
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty());
    assert!(screen(&app, 100, 30).contains("Cancel run"));
}

#[test]
fn editing_the_text_cancels_the_preview() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let effects = press(&mut app, &mut clock, KeyCode::Char('x'), 400);
    assert!(effects.is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("/cancelx"));
    assert!(!screen(&app, 100, 30).contains("Cancel run"));
    // Enter now routes `/cancelx`, which is no command.
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty());
    assert!(screen(&app, 100, 30).contains("no command"));
}

#[test]
fn arming_a_preview_asks_the_loop_to_drop_the_keys_that_are_queued() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    assert!(!app.take_flush());
    send(&mut app, &mut clock, "/cancel");
    assert!(app.take_flush(), "the loop was not asked to drain the keys");
    assert!(!app.take_flush(), "the request is made once");
    // A command that runs at once arms nothing.
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    assert!(!app.take_flush());
}

#[test]
fn a_burst_of_enters_around_the_preview_cannot_confirm_it_until_the_quiet_time_has_passed() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    // 200 ms after the preview: too soon. 10 ms later: a burst (two keys 10 ms apart), which also starts the 150 ms quiet time.
    assert!(enter(&mut app, &mut clock, 200).is_empty());
    assert!(enter(&mut app, &mut clock, 10).is_empty());
    // 110 ms later the preview is older than 300 ms, but the quiet time of the burst runs until 150 ms after the second Enter.
    assert!(
        enter(&mut app, &mut clock, 110).is_empty(),
        "a key inside the quiet time confirmed"
    );
    // 80 ms after that the quiet time is over: this Enter is calm and confirms.
    let effects = enter(&mut app, &mut clock, 120);
    assert_eq!(run_command(&effects), argv(&["cancel", RUN]));
}

#[test]
fn a_paste_after_the_preview_replaces_it_and_the_next_enter_previews_the_pasted_text() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let at = clock.after(400);
    assert!(app.handle_paste_at("x".to_string(), at).is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("/cancelx"));
    assert!(!screen(&app, 100, 30).contains("Cancel run"));
}

// ---- typed answers -------------------------------------------------------------------------------------------------------------

#[test]
fn a_security_setting_asks_for_its_key_name_before_it_runs() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(
        &mut app,
        &mut clock,
        "/config set api_endpoint https://api.example.com/v1",
    );
    assert!(effects.is_empty());
    let text = screen(&app, 100, 30);
    assert!(text.contains("api_endpoint"), "{text}");
    // Enter before anything is typed does not run it.
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty());
    assert!(screen(&app, 100, 30).contains("Type api_endpoint"));
    // A wrong answer does not run it either.
    type_text(&mut app, &mut clock, "api_endpoin");
    let effects = enter(&mut app, &mut clock, 10);
    assert!(effects.is_empty(), "{effects:?}");
    // The right one does (ASCII case is ignored).
    type_text(&mut app, &mut clock, "T");
    let effects = enter(&mut app, &mut clock, 10);
    assert_eq!(
        run_command(&effects),
        argv(&[
            "config",
            "set",
            "api_endpoint",
            "https://api.example.com/v1"
        ])
    );
}

#[test]
fn backspace_edits_the_typed_answer_and_the_box_text_is_not_touched() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let line = "/config set api_endpoint https://a.example/v1";
    send(&mut app, &mut clock, line);
    // Characters typed less than 300 ms after the preview are ignored like an Enter would be.
    clock.after(400);
    type_text(&mut app, &mut clock, "api_endpointZ");
    press(&mut app, &mut clock, KeyCode::Backspace, 50);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(line),
        "the answer went into the box"
    );
    let effects = enter(&mut app, &mut clock, 100);
    assert_eq!(
        run_command(&effects),
        argv(&["config", "set", "api_endpoint", "https://a.example/v1"])
    );
}

// ---- pasted text ------------------------------------------------------------------------------------------------------------------

#[test]
fn pasted_text_never_runs_without_the_preview() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let at = clock.after(100);
    assert!(app.handle_paste_at("/runs".to_string(), at).is_empty());
    // Past the quiet time after the paste, Enter shows the preview even for a command that only reads.
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty(), "pasted /runs ran at once: {effects:?}");
    assert!(screen(&app, 100, 30).contains("ham runs"));
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["runs"]));
}

#[test]
fn the_taint_lasts_until_the_box_is_emptied() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let at = clock.after(100);
    app.handle_paste_at("x".to_string(), at);
    // Emptying the box ends the taint: a typed /runs runs at once.
    app.handle_key_at(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        clock.after(400),
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
    let effects = send(&mut app, &mut clock, "/runs");
    assert_eq!(run_command(&effects), argv(&["runs"]));
}

#[test]
fn a_pasted_tail_after_typed_text_taints_the_whole_draft() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/runs ");
    let at = clock.after(100);
    app.handle_paste_at("--probes".to_string(), at);
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app, 100, 30).contains("ham runs --probes"));
}

// ---- commands that need the terminal ---------------------------------------------------------------------------------------------

#[test]
fn a_push_shows_its_preview_and_the_second_enter_hands_the_terminal_to_it() {
    let url = "https://git.example.com/team/app.git";
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, &format!("/push {url}"));
    assert!(
        effects.is_empty(),
        "ran without a second Enter: {effects:?}"
    );
    let text = screen(&app, 140, 30);
    assert!(
        !text.contains("needs your terminal"),
        "the push is no longer refused:\n{text}"
    );
    assert!(text.contains(&format!("Push run {RUN} to {url}")), "{text}");
    assert_eq!(
        app.chat_text().as_deref(),
        Some(format!("/push {url}").as_str())
    );
    // Enter before the last four characters of the run id are typed does not run it.
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty(), "{effects:?}");
    type_text(&mut app, &mut clock, &RUN[RUN.len() - 4..]);
    let effects = enter(&mut app, &mut clock, 10);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["push", RUN, url])
        }]
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

#[test]
fn rules_edit_shows_its_preview_and_the_second_enter_hands_the_terminal_to_the_editor() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/rules edit");
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 140, 30);
    assert!(!text.contains("needs your terminal"), "{text}");
    assert!(text.contains("Open your editor"), "{text}");
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["rules", "edit"])
        }]
    );
}

#[test]
fn esc_cancels_the_preview_of_a_command_that_needs_the_terminal_and_nothing_runs() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/rules edit");
    assert!(screen(&app, 140, 30).contains("Open your editor"));
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("/rules edit"));
    assert!(!screen(&app, 140, 30).contains("Open your editor"));
}

#[test]
fn what_a_terminal_command_wrote_shows_in_the_panel_with_its_exit_code() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/rules edit");
    enter(&mut app, &mut clock, 400);
    app.update(Msg::CommandDone {
        argv: argv(&["rules", "edit"]),
        out: out("no editor: set $VISUAL or $EDITOR\n", 1, None),
    });
    let text = screen(&app, 140, 30);
    assert!(text.contains("no editor: set $VISUAL or $EDITOR"), "{text}");
    assert!(text.contains("exit 1"), "{text}");
}

#[test]
fn rules_show_needs_no_terminal_and_runs() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/rules show");
    assert_eq!(run_command(&effects), argv(&["rules", "show"]));
}

// ---- words of the screen itself ---------------------------------------------------------------------------------------------------

#[test]
fn slash_new_opens_the_new_run_form() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/new");
    assert!(effects.is_empty());
    assert_eq!(app.screen(), Screen::Composer);
}

#[test]
fn slash_open_and_slash_watch_open_a_run_and_run_nothing() {
    for line in [format!("/open {RUN2}"), format!("/watch {RUN2}")] {
        let mut app = ready(false);
        let mut clock = Clock::new();
        let effects = send(&mut app, &mut clock, &line);
        assert!(
            effects
                .iter()
                .all(|e| !matches!(e, Effect::RunCommand { .. })),
            "{line}: {effects:?}"
        );
        assert_eq!(app.screen(), Screen::Dashboard);
        assert_eq!(app.focused_run(), Some(RUN2), "{line}");
    }
}

#[test]
fn slash_help_lists_the_commands_in_the_panel() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/help");
    assert!(effects.is_empty());
    let text = screen(&app, 100, 40);
    assert!(text.contains("Commands"), "{text}");
    assert!(text.contains("/cancel"), "{text}");
}

#[test]
fn slash_help_with_a_word_explains_that_command() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/help push");
    let text = screen(&app, 100, 40);
    assert!(text.contains("push"), "{text}");
    assert!(text.contains("git-url"), "{text}");
}

#[test]
fn a_start_form_typed_in_the_box_opens_the_new_run_form_filled_in_and_starts_nothing() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(
        &mut app,
        &mut clock,
        "ham \"build a todo api\" --s3-only --direct --input-wait 5",
    );
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 100, 40);
    assert!(text.contains("build a todo api"), "{text}");
    assert!(text.contains("--input-wait 5"), "{text}");
    // Ctrl-S in the form is the person's own act; it carries the prefilled values.
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("expected a submit from the form, got {effects:?}");
    };
    assert_eq!(args.prompt, "build a todo api");
    assert_eq!(args.input_wait, Some(5));
}

// ---- results ----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_text_of_a_finished_command_shows_in_the_panel_and_esc_closes_it() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out(
            "01K6A0000000000000000000R1  active  s3-only\nsecond line\n",
            0,
            None,
        ),
    });
    let text = screen(&app, 100, 30);
    assert!(
        text.contains("01K6A0000000000000000000R1  active"),
        "{text}"
    );
    assert!(text.contains("second line"), "{text}");
    // Esc in typing mode leaves typing, and the next Esc closes the panel; it never goes to the Runs list while a panel is open.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert_eq!(app.mode(), Mode::Browse);
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(!screen(&app, 100, 30).contains("second line"));
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn a_failed_command_says_so_in_the_panel_title() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: CmdOutput {
            chunks: vec![Chunk {
                stream: Stream::Err,
                text: "error: the stage said no\n".into(),
            }],
            code: 1,
            bearer: None,
        },
    });
    let text = screen(&app, 100, 30);
    assert!(text.contains("the stage said no"), "{text}");
    assert!(text.contains("exit 1"), "{text}");
}

#[test]
fn control_characters_in_a_command_result_are_not_drawn() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out("plain\x1b[1m\x1b]0;windowname\x07text\n", 0, None),
    });
    let text = screen(&app, 100, 30);
    assert!(text.contains("plain"), "{text}");
    assert!(!text.contains('\x1b'), "{text:?}");
    assert!(!text.contains("windowname"), "{text}");
}

#[test]
fn a_long_result_is_cut_to_the_panel_and_scrolls() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    let many: String = (1..=60).map(|i| format!("line {i:02}\n")).collect();
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out(&many, 0, None),
    });
    press(&mut app, &mut clock, KeyCode::Esc, 50); // browse mode
    let text = screen(&app, 100, 30);
    assert!(text.contains("line 01"), "{text}");
    assert!(!text.contains("line 60"), "{text}");
    for _ in 0..3 {
        press(&mut app, &mut clock, KeyCode::PageDown, 50);
    }
    let text = screen(&app, 100, 30);
    assert!(!text.contains("line 01"), "{text}");
    assert!(text.contains("line 3") || text.contains("line 4"), "{text}");
}

#[test]
fn a_bearer_link_is_never_drawn_and_opens_only_on_the_users_o() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/link");
    app.update(Msg::CommandDone {
        argv: argv(&["link", RUN]),
        out: out(
            "Download link ready, expires 06:12 MDT; u copies, o opens\n",
            0,
            Some(LINK),
        ),
    });
    let text = screen(&app, 140, 30);
    assert!(text.contains("Download link ready"), "{text}");
    assert!(!text.contains("secret-link"), "the link was drawn:\n{text}");
    assert!(
        !format!("{app:?}").contains("secret-link"),
        "the link is in the debug text of the app"
    );
    press(&mut app, &mut clock, KeyCode::Esc, 50); // browse mode
    let effects = press(&mut app, &mut clock, KeyCode::Char('o'), 50);
    let [Effect::OpenBearer { bearer, what, .. }] = effects.as_slice() else {
        panic!("expected one OpenBearer, got {effects:?}");
    };
    assert_eq!(bearer.url(), LINK);
    assert_eq!(*what, "download link");
    assert!(
        !format!("{effects:?}").contains("secret-link"),
        "the link is in the debug text of the effect"
    );
    // `u` has no clipboard writer yet: it says so and leaves the link alone.
    let effects = press(&mut app, &mut clock, KeyCode::Char('u'), 50);
    assert!(effects.is_empty());
    assert!(screen(&app, 140, 30).contains("copying"));
}

#[test]
fn a_security_setting_that_was_changed_says_it_takes_effect_after_a_restart() {
    let mut app = ready(false);
    app.update(Msg::CommandDone {
        argv: argv(&[
            "config",
            "set",
            "api_endpoint",
            "https://api.example.com/v1",
        ]),
        out: out("api_endpoint = https://api.example.com/v1\n", 0, None),
    });
    assert!(screen(&app, 120, 30).contains("restart"));
}

#[test]
fn any_setting_that_was_changed_says_the_running_app_keeps_the_old_one() {
    let mut app = ready(false);
    app.update(Msg::CommandDone {
        argv: argv(&["config", "set", "default_model", "sol"]),
        out: out("default_model = sol\n", 0, None),
    });
    assert!(screen(&app, 120, 30).contains("restart"));
    // A failed change says nothing of the kind.
    let mut app = ready(false);
    app.update(Msg::CommandDone {
        argv: argv(&["config", "set", "default_model", "nope"]),
        out: out("error: no such model\n", 1, None),
    });
    assert!(!screen(&app, 120, 30).contains("restart"));
}

// ---- the preview freezes more than the argv -------------------------------------------------------------------------------------------

static REVISION: AtomicU64 = AtomicU64::new(1);

fn revision() -> u64 {
    REVISION.load(Ordering::SeqCst)
}

#[test]
fn a_change_of_the_config_file_after_the_preview_stops_the_command() {
    REVISION.store(1, Ordering::SeqCst);
    let mut app = ready(false);
    app.set_config_revision(revision);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    REVISION.store(2, Ordering::SeqCst);
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        effects.is_empty(),
        "ran after the settings changed: {effects:?}"
    );
    let text = screen(&app, 100, 30);
    assert!(text.contains("settings changed"), "{text}");
    assert!(
        !text.contains("Cancel run"),
        "the stale preview stayed:\n{text}"
    );
    // The box still holds the text, so one more Enter makes a fresh preview and a second one runs it.
    send_enter_twice_runs_cancel(&mut app, &mut clock);
}

fn send_enter_twice_runs_cancel(app: &mut App, clock: &mut Clock) {
    let effects = enter(app, clock, 400);
    assert!(effects.is_empty());
    let effects = enter(app, clock, 400);
    assert_eq!(run_command(&effects), argv(&["cancel", RUN]));
}

#[test]
fn the_preview_belongs_to_the_run_it_named() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    app.open_run(RUN2);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    for _ in 0..2 {
        let effects = enter(&mut app, &mut clock, 400);
        assert!(
            effects
                .iter()
                .all(|e| !matches!(e, Effect::RunCommand { .. })),
            "a preview of run 1 ran on run 2: {effects:?}"
        );
    }
}

// ---- review findings ---------------------------------------------------------------------

#[test]
fn a_key_with_no_instant_cannot_confirm_a_preview_made_a_moment_ago() {
    let mut app = ready(false);
    for c in "/cancel".chars() {
        assert!(app.handle_key(key(KeyCode::Char(c))).is_empty());
    }
    assert!(
        app.handle_key(key(KeyCode::Enter)).is_empty(),
        "the preview did not arm"
    );
    // `handle_key` carries no instant: it is read at the real time, which is far less than 300 ms after the preview.
    let effects = app.handle_key(key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "a key without an instant confirmed: {effects:?}"
    );
}

#[test]
fn a_paste_that_arrives_as_fast_keys_taints_the_draft() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    // `/runs` as keys 5 ms apart, the way a terminal without bracketed paste sends a paste; then the person's own calm Enter.
    for c in "/runs".chars() {
        app.handle_key_at(key(KeyCode::Char(c)), clock.after(5));
    }
    assert!(app.draft_is_tainted());
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        effects.is_empty(),
        "fast keys ran a command at once: {effects:?}"
    );
    assert!(screen(&app, 100, 30).contains("ham runs"));
}

#[test]
fn typing_at_a_human_pace_does_not_taint_the_draft() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/runs");
    assert!(!app.draft_is_tainted());
}

#[test]
fn alt_and_a_letter_while_a_preview_is_up_only_cancel_it() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let alt_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT);
    let effects = app.handle_key_at(alt_c, clock.after(400));
    assert!(effects.is_empty());
    assert_eq!(app.mode(), Mode::Typing, "the letter acted as a hotkey");
    assert_eq!(app.chat_text().as_deref(), Some("/cancel"));
    assert!(!screen(&app, 100, 30).contains("Cancel run"));
}

#[test]
fn the_bearer_link_opens_only_on_a_calm_unmodified_o() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/link");
    app.update(Msg::CommandDone {
        argv: argv(&["link", RUN]),
        out: out("Download link ready\n", 0, Some(LINK)),
    });
    press(&mut app, &mut clock, KeyCode::Esc, 50); // browse mode
    let alt_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);
    assert!(app.handle_key_at(alt_o, clock.after(300)).is_empty());
    // An `o` that follows another key within 30 ms is part of a burst.
    app.handle_key_at(key(KeyCode::Char('x')), clock.after(300));
    assert!(
        app.handle_key_at(key(KeyCode::Char('o')), clock.after(5))
            .is_empty(),
        "an o inside a burst opened the link"
    );
    let effects = press(&mut app, &mut clock, KeyCode::Char('o'), 400);
    assert!(
        matches!(effects.as_slice(), [Effect::OpenBearer { .. }]),
        "{effects:?}"
    );
}

// ---- small screens, wrapping, the cursor -------------------------------------------------------------------------------------------

/// The screen as text and the terminal's cursor (column, row).
fn draw(app: &App, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
    (screen(app, w, h), (pos.x, pos.y))
}

fn row_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on the screen:\n{text}"))
}

#[test]
fn the_preview_fits_at_80_by_24_with_the_box_and_the_footer_and_the_cursor_stays_in_the_box() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/cancel");
    let (text, (_, y)) = draw(&app, 80, 24);
    assert!(text.contains("Cancel run"), "{text}");
    let box_row = row_of(&text, "Message to Cline");
    assert!(text.contains("Enter send"), "the footer is gone:\n{text}");
    assert!(
        usize::from(y) > box_row && usize::from(y) < row_of(&text, "Enter send"),
        "the cursor ({y}) is not in the box (title row {box_row}):\n{text}"
    );
    assert!(row_of(&text, "Cancel run") < box_row, "{text}");
}

#[test]
fn the_cursor_is_in_the_preview_while_it_waits_for_a_typed_answer() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(
        &mut app,
        &mut clock,
        "/config set api_endpoint https://api.example.com/v1",
    );
    clock.after(400);
    type_text(&mut app, &mut clock, "api");
    let (text, (x, y)) = draw(&app, 100, 30);
    let row = row_of(&text, "> api");
    assert_eq!(usize::from(y), row, "{text}");
    let line = text.lines().nth(row).unwrap();
    let col = line[..line.find("> api").unwrap()].chars().count() + "> api".len();
    assert_eq!(usize::from(x), col, "{text}");
}

#[test]
fn a_long_refusal_wraps_inside_the_panel_at_80_columns() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "//hello");
    let text = screen(&app, 80, 24);
    assert!(text.contains("use /send <text>"), "{text}");
    assert!(text.contains("Message to Cline"), "{text}");
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "{line:?}");
    }
}

#[test]
fn the_help_panel_never_pushes_the_box_off_a_small_screen() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/help");
    let (text, (_, y)) = draw(&app, 80, 24);
    assert!(text.contains("Commands"), "{text}");
    assert!(text.contains("Message to Cline"), "{text}");
    assert!(text.contains("Enter send"), "{text}");
    assert!(usize::from(y) > row_of(&text, "Message to Cline"), "{text}");
}

// ---- the effect against the stage -----------------------------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

async fn started_run(a: &Api<HttpsPlane>, dir: &std::path::Path) -> String {
    started_run_with(a, dir, "router-1").await
}

/// A run started with its own launch nonce (the same nonce is the same run).
async fn started_run_with(a: &Api<HttpsPlane>, dir: &std::path::Path, nonce: &str) -> String {
    let words = argv(&[
        "--prompt",
        "build x",
        "--s3-only",
        "--no-rules",
        "--direct",
        "--model",
        ham_core::models::DEFAULT_MODEL_ID,
    ]);
    let ham_core::command::Command::StartDirect(args) = parse(&words).unwrap().command else {
        panic!("not a start");
    };
    let msg = perform(
        a,
        dir,
        Effect::SubmitDirect {
            args,
            nonce: nonce.into(),
        },
    )
    .await
    .unwrap();
    let Msg::Started(Ok(r)) = msg else {
        panic!("the run did not start: {msg:?}");
    };
    r.run_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_effect_runs_the_real_command_against_the_stage_and_returns_its_text() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-runs");
    let run = started_run(&a, &dir).await;
    let words = argv(&["runs"]);
    let msg = perform(
        &a,
        &dir,
        Effect::RunCommand {
            invocation: parse(&words).unwrap(),
            argv: words.clone(),
        },
    )
    .await
    .unwrap();
    let Msg::CommandDone { argv: got, out } = msg else {
        panic!("not a command result: {msg:?}");
    };
    assert_eq!(got, words);
    assert_eq!(out.code, 0, "{:?}", out.stderr_text());
    assert!(out.stdout_text().contains(&run), "{}", out.stdout_text());
    assert_eq!(out.bearer, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirmed_cancel_reaches_the_stage_as_a_signed_post() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-cancel");
    let run = started_run(&a, &dir).await;
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(&run);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('i'), 50);
    type_text(&mut app, &mut clock, "/cancel");
    assert!(enter(&mut app, &mut clock, 10).is_empty());
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["cancel", &run]));
    let msg = perform(&a, &dir, effects.into_iter().next().unwrap())
        .await
        .unwrap();
    let Msg::CommandDone { out, .. } = &msg else {
        panic!("not a command result: {msg:?}");
    };
    assert_eq!(out.code, 0, "{:?}", out.stderr_text());
    assert!(out.stdout_text().contains(&format!("run {run}")));
    app.update(msg);
    let posts = fake.mem.posts(&format!("/v1/runs/{run}/cancel"));
    assert_eq!(posts.len(), 1, "{:?}", fake.mem.seen());
    assert_eq!(posts[0].signature, Ok(()));
}

// ---- the branches the first tests did not reach -----------------------------------------------------------------------------------

#[test]
fn a_push_without_its_url_is_no_longer_refused_it_asks_for_the_url() {
    // Retired the refusal 'needs its <git-url>: type the whole command' (this test pinned it): the box opens a preview with
    // an editable URL slot (`tests/tui_push_slot.rs` holds the whole flow).
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/push");
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 100, 30);
    assert!(!text.contains("needs its <git-url>"), "{text}");
    assert!(!text.contains("needs your terminal"), "{text}");
    assert!(text.contains("git URL"), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("/push"));
}

#[test]
fn a_start_line_with_a_repository_a_base_a_model_and_odd_file_names_fills_the_form() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(
        &mut app,
        &mut clock,
        "ham \"build x\" --repo cc:my-repo --base main --direct --model sol --rules \"my rules\" --continue 'say \"hi\".md' --input-wait 0",
    );
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 140, 40);
    assert!(text.contains("cc:my-repo"), "{text}");
    assert!(text.contains("main"), "{text}");
    assert!(text.contains("GPT-6.1 Sol"), "{text}");
    assert!(text.contains("--rules \"my rules\""), "{text}");
    assert!(text.contains("--continue 'say \"hi\".md'"), "{text}");
    assert!(text.contains("--input-wait 0"), "{text}");
}

#[test]
fn a_start_line_for_a_new_repository_and_a_file_name_with_an_apostrophe() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(
        &mut app,
        &mut clock,
        "ham \"build x\" --new-repo cc:fresh --direct --continue \"it's.md\"",
    );
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 140, 40);
    assert!(text.contains("cc:fresh"), "{text}");
    assert!(text.contains("--continue \"it's.md\""), "{text}");
    assert!(text.contains("(•) --new-repo"), "{text}");
}

#[test]
fn a_start_line_with_a_machine_choice_fills_the_options_box_and_the_form_sends_it() {
    // Every word, `auto` too: an explicit `--compute auto` wins over `run.compute` of the file, so the form must keep it.
    for word in ["auto", "microvm", "instances"] {
        let mut app = ready(false);
        let mut clock = Clock::new();
        let effects = send(
            &mut app,
            &mut clock,
            &format!("ham \"build x\" --s3-only --direct --compute {word}"),
        );
        assert!(effects.is_empty(), "{word}: {effects:?}");
        assert_eq!(app.screen(), Screen::Composer, "{word}");
        let text = screen(&app, 140, 40);
        assert!(
            text.contains(&format!("--compute {word}")),
            "{word}:\n{text}"
        );
        // Ctrl-S in the form is the person's own act; it carries the machine that was typed.
        let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
            panic!("{word}: expected a submit from the form, got {effects:?}");
        };
        assert_eq!(args.compute.map(|c| c.word()), Some(word));
    }
    // TRIGGER PRESENT: the typed line did carry the flag into the form (the parser accepts it).
    let line = parse(
        &["build x", "--s3-only", "--direct", "--compute", "instances"].map(ToString::to_string),
    )
    .unwrap();
    assert!(
        matches!(line.command, ham_core::command::Command::StartDirect(ref a) if a.compute.is_some())
    );
    println!("START-FORM-COMPUTE TRIGGER PRESENT: the typed start line parses with compute=Some");
}

#[test]
fn a_start_line_with_a_model_that_does_not_exist_says_so_in_the_form() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(
        &mut app,
        &mut clock,
        "ham \"build x\" --s3-only --direct --model nosuchmodel",
    );
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 140, 40);
    assert!(text.contains("--model nosuchmodel"), "{text}");
}

#[test]
fn the_arrow_keys_and_page_keys_scroll_a_long_panel_both_ways() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    let many: String = (1..=60).map(|i| format!("line {i:02}\n")).collect();
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out(&many, 0, None),
    });
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert!(screen(&app, 100, 30).contains("line 01"));
    press(&mut app, &mut clock, KeyCode::Down, 50);
    let text = screen(&app, 100, 30);
    assert!(
        !text.contains("line 01") && text.contains("line 02"),
        "{text}"
    );
    press(&mut app, &mut clock, KeyCode::PageDown, 50);
    press(&mut app, &mut clock, KeyCode::Up, 50);
    press(&mut app, &mut clock, KeyCode::PageUp, 50);
    let text = screen(&app, 100, 30);
    assert!(
        text.contains("line 01") || text.contains("line 02"),
        "{text}"
    );
    // Past the end it stops at the last page.
    for _ in 0..20 {
        press(&mut app, &mut clock, KeyCode::PageDown, 50);
    }
    let text = screen(&app, 100, 30);
    assert!(text.contains("line 60"), "{text}");
}

#[test]
fn a_table_line_wider_than_the_panel_is_cut_with_an_ellipsis_and_a_long_word_is_split() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    let wide = format!("{}\n", "w".repeat(300));
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out(&wide, 0, None),
    });
    let text = screen(&app, 80, 24);
    assert!(text.contains('…'), "{text}");
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "{line:?}");
    }
    // A refusal wraps; a word longer than a row is split at the edge and nothing is lost.
    let mut app = ready(false);
    let mut clock = Clock::new();
    let word = "z".repeat(150);
    send(&mut app, &mut clock, &format!("/{word}"));
    let text = screen(&app, 80, 24);
    let zs: usize = text
        .lines()
        .filter(|l| l.starts_with('│'))
        .map(|l| l.matches('z').count())
        .sum();
    assert!(
        zs >= 150,
        "only {zs} of the 150 letters are on the screen:\n{text}"
    );
}

#[test]
fn the_debug_text_of_a_command_output_hides_the_bearer_link() {
    let o = out("Download link ready\n", 0, Some(LINK));
    let dump = format!("{o:?}");
    assert!(!dump.contains("secret-link"), "{dump}");
    assert!(dump.contains("hidden"), "{dump}");
    assert!(format!("{:?}", out("x\n", 0, None)).contains("None"));
}

#[test]
fn the_config_revision_is_a_stable_number() {
    let a = ham::tui::commands::config_revision();
    assert_eq!(a, ham::tui::commands::config_revision());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_opener_gets_the_bearer_link_and_its_answers_never_name_it() {
    let fake = Fake::start().await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-open");
    let effect = |program: &str| Effect::OpenBearer {
        program: program.into(),
        bearer: ham::tui::commands::Bearer::new(LINK.into()),
        what: "download link",
    };
    // `true` ends with success whatever it is given.
    let msg = perform(&a, &dir, effect("true")).await.unwrap();
    let Msg::Opened(Ok(opened)) = msg else {
        panic!("not opened: {msg:?}");
    };
    assert_eq!(opened.what, "download link");
    // A program that cannot run gives a message without the link.
    let msg = perform(&a, &dir, effect("/nonexistent/opener"))
        .await
        .unwrap();
    let Msg::Opened(Err(why)) = msg else {
        panic!("expected an error, got {msg:?}");
    };
    assert!(!why.contains("secret-link"), "{why}");
}

#[test]
fn a_paste_on_the_reply_screen_is_text_and_ctrl_c_clears_it() {
    let mut app = ready(true);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50); // browse mode
    press(&mut app, &mut clock, KeyCode::Char('p'), 50);
    assert_eq!(app.screen(), Screen::Reply);
    assert!(app.handle_paste("pasted\r\nanswer".to_string()).is_empty());
    let text = screen(&app, 100, 30);
    assert!(text.contains("pasted") && text.contains("answer"), "{text}");
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.handle_key_at(ctrl_c, clock.after(300)).is_empty());
    assert!(!screen(&app, 100, 30).contains("pasted"));
}

#[test]
fn a_paste_on_the_artifacts_screen_is_ignored_and_the_wheel_still_scrolls_it() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('a'), 50);
    assert_eq!(app.screen(), Screen::Artifacts);
    assert!(app.handle_paste("x".to_string()).is_empty());
    assert!(app.handle_wheel(true).is_empty());
    assert!(app.handle_wheel(false).is_empty());
    assert_eq!(app.screen(), Screen::Artifacts);
}

#[test]
fn a_paste_goes_to_the_base_ref_and_options_boxes_of_the_composer_and_ctrl_c_clears_them() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let mut clock = Clock::early();
    press(&mut app, &mut clock, KeyCode::Char('n'), 50);
    assert_eq!(app.screen(), Screen::Composer);
    // Prompt, Target, Repository, Base ref.
    for _ in 0..3 {
        press(&mut app, &mut clock, KeyCode::Tab, 50);
    }
    app.handle_paste("origin/main".to_string());
    assert!(screen(&app, 100, 40).contains("origin/main"));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_key_at(ctrl_c, clock.after(300));
    assert!(!screen(&app, 100, 40).contains("origin/main"));
    press(&mut app, &mut clock, KeyCode::Tab, 50); // Options
    app.handle_paste("--no-rules".to_string());
    let text = screen(&app, 100, 40);
    assert!(text.contains("--no-rules"), "{text}");
}

// ---- the event loop drops the keys queued behind the Enter that armed a preview ---------------------------------------------------

/// Stamped keys for the loop: a person's pace unless the test says otherwise.
fn stamped(base: Instant, ms: u64, code: KeyCode) -> (crossterm::event::Event, Instant) {
    (
        crossterm::event::Event::Key(key(code)),
        base + Duration::from_millis(ms),
    )
}

async fn run_loop(
    fake: &Fake,
    run: &str,
    events: Vec<(crossterm::event::Event, Instant)>,
    later: Option<(Duration, (crossterm::event::Event, Instant))>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for e in events {
        tx.send(e).unwrap();
    }
    let sender = tokio::spawn(async move {
        if let Some((wait, e)) = later {
            tokio::time::sleep(wait).await;
            tx.send(e).unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // The sender ends here: the loop ends when it has read every event.
    });
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_run_opens_typing(true);
    app.open_run(run);
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let a = api(fake);
    let dir = support::scratch("tui-router-loop");
    ham::tui::event_loop_timed(
        &mut term,
        a,
        dir,
        app,
        vec![],
        rx,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    sender.await.unwrap();
    // Let the spawned command finish its request.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn typed(base: Instant, text: &str) -> Vec<(crossterm::event::Event, Instant)> {
    text.chars()
        .enumerate()
        .map(|(i, c)| stamped(base, 100 + 50 * i as u64, KeyCode::Char(c)))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_queued_behind_the_enter_that_armed_a_preview_are_dropped() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-flush");
    let run = started_run(&a, &dir).await;
    let base = Instant::now();
    let mut events = vec![(crossterm::event::Event::FocusGained, base)];
    events.extend(typed(base, "/cancel"));
    // The Enter that arms the preview, then an Enter 700 ms later that is queued behind it: it would confirm, and it is dropped.
    events.push(stamped(base, 1000, KeyCode::Enter));
    events.push(stamped(base, 1700, KeyCode::Enter));
    events.push((crossterm::event::Event::Resize(100, 30), base));
    run_loop(&fake, &run, events, None).await;
    assert!(
        fake.mem.posts(&format!("/v1/runs/{run}/cancel")).is_empty(),
        "a queued Enter confirmed the preview: {:?}",
        fake.mem.seen()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enter_that_arrives_after_the_preview_was_drawn_confirms_it() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-late");
    let run = started_run(&a, &dir).await;
    let base = Instant::now();
    let mut events = typed(base, "/cancel");
    events.push(stamped(base, 1000, KeyCode::Enter));
    // Sent after the loop handled the Enter that armed the preview.
    let later = (
        Duration::from_millis(400),
        stamped(base, 1700, KeyCode::Enter),
    );
    run_loop(&fake, &run, events, Some(later)).await;
    assert_eq!(
        fake.mem.posts(&format!("/v1/runs/{run}/cancel")).len(),
        1,
        "{:?}",
        fake.mem.seen()
    );
}

// ---- the panel and the focused box ------------------------------------------------------------

fn long_panel(app: &mut App, clock: &mut Clock) {
    send(app, clock, "/runs");
    let many: String = (1..=60).map(|i| format!("line {i:02}\n")).collect();
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out(&many, 0, None),
    });
}

#[test]
fn page_keys_scroll_a_long_panel_while_the_box_has_the_focus() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    long_panel(&mut app, &mut clock);
    assert_eq!(app.mode(), Mode::Typing, "the box keeps the focus");
    let text = screen(&app, 100, 30);
    assert!(text.contains("line 01") && text.contains("1-"), "{text}");
    press(&mut app, &mut clock, KeyCode::PageDown, 50);
    let text = screen(&app, 100, 30);
    assert!(
        !text.contains("line 01"),
        "PageDown moved the panel:\n{text}"
    );
    assert_eq!(app.mode(), Mode::Typing, "and the box still has the focus");
    press(&mut app, &mut clock, KeyCode::PageUp, 50);
    let text = screen(&app, 100, 30);
    assert!(text.contains("line 01"), "PageUp moved it back:\n{text}");
}

#[test]
fn the_page_keys_leave_the_draft_in_the_box_alone() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    long_panel(&mut app, &mut clock);
    type_text(&mut app, &mut clock, "a draft");
    press(&mut app, &mut clock, KeyCode::PageDown, 50);
    press(&mut app, &mut clock, KeyCode::PageUp, 50);
    assert_eq!(app.chat_text().as_deref(), Some("a draft"));
}

#[test]
fn a_panel_that_fits_ignores_the_page_keys_and_stays_open() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/runs");
    app.update(Msg::CommandDone {
        argv: argv(&["runs"]),
        out: out("one short line\n", 0, None),
    });
    press(&mut app, &mut clock, KeyCode::PageDown, 50);
    let text = screen(&app, 100, 30);
    assert!(text.contains("one short line"), "{text}");
}

#[test]
fn in_typing_mode_the_panel_names_only_the_keys_that_scroll_it() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    long_panel(&mut app, &mut clock);
    let text = screen(&app, 100, 30);
    assert!(text.contains("PgUp PgDn scroll"), "{text}");
    assert!(
        !text.contains("\u{2191}\u{2193} PgUp"),
        "the arrow keys belong to the box while it has the focus:\n{text}"
    );
    assert!(text.contains("Esc Esc closes"), "{text}");
    // In browse mode the arrows scroll the panel and the hint says so.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let text = screen(&app, 100, 30);
    assert!(text.contains("\u{2191}\u{2193} PgUp PgDn scroll"), "{text}");
}

// ---- commands that hand the terminal over, and drafts that come back ---------------------------------------

#[test]
fn continue_without_a_prompt_previews_and_the_second_enter_hands_the_terminal_to_it() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/continue");
    assert!(
        effects.is_empty(),
        "ran without a second Enter: {effects:?}"
    );
    let text = screen(&app, 140, 30);
    assert!(!text.contains("needs your terminal"), "{text}");
    assert!(text.contains(&format!("ham continue {RUN}")), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("/continue"));
    // A second Enter inside 300 ms of the preview does nothing; the one after it hands the terminal to a child `ham continue <run>`, which
    // opens the editor for the prompt (the effect is the shell's own argv).
    assert!(enter(&mut app, &mut clock, 100).is_empty());
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["continue", RUN])
        }]
    );
    assert_eq!(app.chat_text().as_deref(), Some(""));
    // The child's text comes back in the panel with its exit code.
    app.update(Msg::CommandDone {
        argv: argv(&["continue", RUN]),
        out: out("started run 01K6A0000000000000000000R9\n", 0, None),
    });
    assert!(screen(&app, 140, 30).contains("started run 01K6A0000000000000000000R9"));
}

/// The in-process runner does not follow a run, so the text form goes to the same child `ham continue <run> <text>` as the one without text:
/// the panel is then what the shell prints. It used to be an in-process command that the runner refused.
#[test]
fn continue_with_a_prompt_hands_the_terminal_to_the_same_child_as_the_one_without() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/continue add the missing tests");
    assert!(effects.is_empty(), "{effects:?}");
    assert!(screen(&app, 140, 30).contains("add the missing tests"));
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(
        effects,
        vec![Effect::RunTerminal {
            argv: argv(&["continue", RUN, "add the missing tests"])
        }]
    );
    // The child's text comes back in the panel with its exit code.
    app.update(Msg::CommandDone {
        argv: argv(&["continue", RUN, "add the missing tests"]),
        out: out("started run 01K6A0000000000000000000R9\n", 0, None),
    });
    assert!(screen(&app, 140, 30).contains("started run 01K6A0000000000000000000R9"));
}

#[test]
fn esc_cancels_the_preview_of_continue_and_nothing_runs() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/continue");
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("/continue"));
    assert!(!screen(&app, 140, 30).contains("ham continue"));
}

#[test]
fn a_draft_that_comes_back_after_a_run_switch_is_still_tainted_and_previews_even_a_read() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    // Run 1: a pasted `/runs` sits in the box.
    let at = clock.after(100);
    app.handle_paste_at("/runs".to_string(), at);
    assert!(app.draft_is_tainted());
    // Run 2: its own box starts empty and clean, and a typed `/runs` there runs at once.
    app.open_run(RUN2);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(
        !app.draft_is_tainted(),
        "the taint followed the person to run 2"
    );
    // Back to run 1: the draft is restored, and it is still the pasted text.
    app.open_run(RUN);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.chat_text().as_deref(), Some("/runs"));
    assert!(app.draft_is_tainted(), "the restored draft lost its taint");
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        effects.is_empty(),
        "a restored pasted /runs ran at once: {effects:?}"
    );
    assert!(screen(&app, 100, 30).contains("ham runs"));
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["runs"]));
}

#[test]
fn a_clean_draft_stays_clean_across_a_run_switch() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "/runs");
    assert!(!app.draft_is_tainted());
    app.open_run(RUN2);
    app.open_run(RUN);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.chat_text().as_deref(), Some("/runs"));
    assert!(!app.draft_is_tainted());
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["runs"]));
}

#[test]
fn a_draft_typed_in_a_burst_keeps_its_taint_across_a_run_switch() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    for c in "/runs".chars() {
        app.handle_key_at(key(KeyCode::Char(c)), clock.after(5));
    }
    assert!(app.draft_is_tainted());
    app.open_run(RUN2);
    app.open_run(RUN);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert!(app.draft_is_tainted());
    let effects = enter(&mut app, &mut clock, 400);
    assert!(effects.is_empty(), "{effects:?}");
}

// ---- `c c` against the stage -------------------------------------------------------------------

/// The text of the line of `screen` that holds `needle`, without the box border, with the run id replaced by `<run>`.
fn line_with(screen: &str, needle: &str, run: &str) -> String {
    screen
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on the screen:\n{screen}"))
        .trim_matches(|c: char| c == '\u{2502}' || c == ' ')
        .replace(run, "<run>")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_c_and_slash_cancel_prepare_the_same_command_and_make_the_same_signed_request() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-router-cc");
    let typed_run = started_run_with(&a, &dir, "router-typed").await;
    let hotkey_run = started_run_with(&a, &dir, "router-hotkey").await;
    assert_ne!(typed_run, hotkey_run);

    // The typed form: /cancel, the preview, the second Enter.
    let mut typed = App::new_direct("n".into(), None, DirectSettings::default());
    typed.open_run(&typed_run);
    let mut clock = Clock::new();
    press(&mut typed, &mut clock, KeyCode::Char('i'), 50);
    type_text(&mut typed, &mut clock, "/cancel");
    assert!(enter(&mut typed, &mut clock, 10).is_empty());
    let typed_preview = line_with(&screen(&typed, 140, 40), "Cancel run", &typed_run);
    let effects = enter(&mut typed, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["cancel", &typed_run]));
    let msg = perform(&a, &dir, effects.into_iter().next().unwrap())
        .await
        .unwrap();
    let Msg::CommandDone { out, .. } = &msg else {
        panic!("not a command result: {msg:?}");
    };
    assert_eq!(out.code, 0, "{:?}", out.stderr_text());

    // The hotkey form: c, the same preview, the second c after 300 ms.
    let mut hot = App::new_direct("n".into(), None, DirectSettings::default());
    hot.open_run(&hotkey_run);
    let mut clock = Clock::new();
    assert!(press(&mut hot, &mut clock, KeyCode::Char('c'), 400).is_empty());
    let hot_preview = line_with(&screen(&hot, 140, 40), "Cancel run", &hotkey_run);
    assert_eq!(hot_preview, typed_preview, "the two previews differ");
    assert!(
        fake.mem
            .posts(&format!("/v1/runs/{hotkey_run}/cancel"))
            .is_empty(),
        "the first c reached the stage"
    );
    let effects = press(&mut hot, &mut clock, KeyCode::Char('c'), 400);
    let [effect @ Effect::Cancel { run_id }] = effects.as_slice() else {
        panic!("expected one cancel, got {effects:?}");
    };
    assert_eq!(run_id, &hotkey_run);
    let msg = perform(&a, &dir, effect.clone()).await.unwrap();
    assert!(matches!(msg, Msg::Cancelled(Ok(_))), "{msg:?}");

    // The same request: one signed POST to the run's cancel route, with the same body.
    let t = fake.mem.posts(&format!("/v1/runs/{typed_run}/cancel"));
    let h = fake.mem.posts(&format!("/v1/runs/{hotkey_run}/cancel"));
    assert_eq!((t.len(), h.len()), (1, 1), "{:?}", fake.mem.seen());
    assert_eq!(t[0].signature, Ok(()));
    assert_eq!(h[0].signature, Ok(()));
    assert_eq!(t[0].body, h[0].body);
    assert_eq!(t[0].query, h[0].query);
}

#[test]
fn b688_a_start_form_typed_with_a_total_time_keeps_it_in_the_form_and_the_request() {
    let mut app = ready(false);
    let mut clock = Clock::new();
    let effects = send(
        &mut app,
        &mut clock,
        "ham \"build a todo api\" --s3-only --direct --max-run-hours 12",
    );
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.screen(), Screen::Composer);
    let text = screen(&app, 100, 40);
    assert!(text.contains("--max-run-hours 12"), "{text}");
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("expected a submit from the form, got {effects:?}");
    };
    assert_eq!(args.max_run_hours, Some(12));
}

// ---- ham update runs from a shell ----------------------------------------------------------------------------------------

#[test]
fn b804_update_typed_in_the_box_asks_for_no_second_enter_and_never_gets_the_terminal() {
    // `ham update` waits for the caller's runs and hands the terminal to setup for up to an hour, so it runs from a shell. Every form typed in
    // the box goes at once to the in-process runner, whose answer is the line that says so (`cli::execute`): no preview asks for a second
    // Enter, and no child `ham` is given the terminal, so nothing is fetched and nothing is deployed from the box.
    for (line, words) in [
        ("/update", vec!["update"]),
        ("/update --yes", vec!["update", "--yes"]),
        ("/update --check", vec!["update", "--check"]),
        ("ham update --no-wait", vec!["update", "--no-wait"]),
        (
            "/update --yes --wait 5",
            vec!["update", "--yes", "--wait", "5"],
        ),
    ] {
        let mut app = ready(false);
        let mut clock = Clock::new();
        let first = send(&mut app, &mut clock, line);
        // A box that armed a preview answers the second Enter: what that Enter starts is part of the evidence.
        let second = if first.is_empty() {
            enter(&mut app, &mut clock, 400)
        } else {
            Vec::new()
        };
        assert!(
            !first
                .iter()
                .chain(&second)
                .any(|e| matches!(e, Effect::RunTerminal { .. })),
            "{line} was handed the terminal: the first Enter gave {first:?}, the second {second:?}"
        );
        assert_eq!(
            run_command(&first),
            argv(&words),
            "{line} did not run at once"
        );
        assert_eq!(app.chat_text().as_deref(), Some(""), "{line}");
        let text = screen(&app, 140, 30);
        assert!(
            !text.contains("Press Enter to run it"),
            "{line} waits for a second Enter:\n{text}"
        );
    }
    // A pasted line is tainted, and a command in tainted text shows its preview before it runs. The answer to `update` is a refusal that
    // runs nothing, so it comes at once for pasted text too, as the refusal of an unknown word does.
    let mut app = ready(false);
    let mut clock = Clock::new();
    let at = clock.after(100);
    app.handle_paste_at("/update --yes".to_string(), at);
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(run_command(&effects), argv(&["update", "--yes"]));
    let text = screen(&app, 140, 30);
    assert!(
        !text.contains("Press Enter to run it"),
        "a pasted /update waits for a second Enter:\n{text}"
    );
}
