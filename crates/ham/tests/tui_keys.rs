//! The key model and the chat box on the dashboard: a run's screen has a message box under its
//! content. In **typing** mode letters are text, so no letter is a command; in **browse** mode the old hotkeys work as before. Esc leaves
//! typing for browse, `i` or Enter returns. Enter sends (to the command sink, then the default route), Ctrl-J, Shift-Enter and backslash
//! then Enter add a line, a paste is text, and keys that arrive as a burst never send. Drafts stay per run for the life of the process.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::{self, Action, EDITOR_KEYS_OVERRIDDEN, Mode, TABLE};
use ham::tui::{App, CommandSink, DirectSettings, Effect, Msg, Screen, Submit};
use harness_protocol::api::v1::{EventsPage, InputAnswer};
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const RUN2: &str = "01K6A0000000000000000000R2";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const OPENED: &str = "2026-10-15T13:41:07Z";
const EXPIRES: &str = "2026-10-15T14:11:07Z";
const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(key(code))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    press(app, KeyCode::Char(c))
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        let effects = ch(app, c);
        assert!(effects.is_empty(), "typing {c:?} caused {effects:?}");
    }
}

/// The screen as text, and where the terminal's cursor is after the draw.
fn draw(app: &App, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let pos = t.backend_mut().get_cursor_position().unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    (out, (pos.x, pos.y))
}

fn screen(app: &App, w: u16, h: u16) -> String {
    draw(app, w, h).0
}

/// A dashboard on `RUN`, in browse mode (the mode a run opens in until the chat view makes typing the default).
fn dashboard() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    assert_eq!(app.screen(), Screen::Dashboard);
    assert_eq!(app.mode(), Mode::Browse);
    app
}

fn typing() -> App {
    let mut app = dashboard();
    ch(&mut app, 'i');
    assert_eq!(app.mode(), Mode::Typing);
    app
}

// ---- the table ----------------------------------------------------------------------------------------------------------------

#[test]
fn no_two_rows_of_the_table_share_a_mode_and_a_key() {
    for (i, a) in TABLE.iter().enumerate() {
        for b in &TABLE[i + 1..] {
            assert!(
                !(a.mode == b.mode && a.chord == b.chord),
                "{:?} and {:?} share {:?} in {:?}",
                a.action,
                b.action,
                a.chord,
                a.mode
            );
        }
    }
}

#[test]
fn shift_tab_has_one_row_and_a_backtab_with_or_without_shift_is_that_row() {
    // crossterm reads ESC [ Z and the CSI-u forms of Shift-Tab as BackTab with Shift; no unix terminal sends BackTab with no modifier. A second row
    // for the plain form was a row no terminal could press, and the battery map kept it as a gap (BAT9). One row serves both forms.
    let rows: Vec<_> = TABLE
        .iter()
        .filter(|r| r.mode == Mode::Browse && r.chord.code == KeyCode::BackTab)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].action, Action::SelectPrev);
    assert_eq!(rows[0].keys, "Shift-Tab");
    for mods in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
        assert_eq!(
            keys::lookup(Mode::Browse, &with(KeyCode::BackTab, mods)),
            Some(Action::SelectPrev),
            "{mods:?}"
        );
    }
    assert_eq!(
        keys::chord(&key(KeyCode::BackTab)),
        keys::chord(&with(KeyCode::BackTab, KeyModifiers::SHIFT))
    );
    // Another modifier is another key, and typing mode has no row for the key.
    assert_eq!(
        keys::lookup(Mode::Browse, &with(KeyCode::BackTab, KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(
        keys::lookup(Mode::Typing, &with(KeyCode::BackTab, KeyModifiers::SHIFT)),
        None
    );
}

#[test]
fn every_key_the_editor_crate_takes_for_itself_and_the_app_uses_has_a_typing_row() {
    for c in EDITOR_KEYS_OVERRIDDEN {
        let e = with(KeyCode::Char(*c), KeyModifiers::CONTROL);
        assert!(
            keys::lookup(Mode::Typing, &e).is_some(),
            "Ctrl-{c} has no row in typing mode"
        );
    }
    // The three rows the plan names: Enter sends, Ctrl-J and Shift-Enter add a line.
    assert_eq!(
        keys::lookup(Mode::Typing, &key(KeyCode::Enter)),
        Some(Action::Send)
    );
    assert_eq!(
        keys::lookup(
            Mode::Typing,
            &with(KeyCode::Char('j'), KeyModifiers::CONTROL)
        ),
        Some(Action::NewLine)
    );
    assert_eq!(
        keys::lookup(Mode::Typing, &with(KeyCode::Enter, KeyModifiers::SHIFT)),
        Some(Action::NewLine)
    );
}

#[test]
fn a_letter_is_never_a_command_in_typing_mode_and_every_old_hotkey_is_in_browse_mode() {
    for c in ('a'..='z').chain('A'..='Z') {
        assert_eq!(
            keys::lookup(Mode::Typing, &key(KeyCode::Char(c))),
            None,
            "{c} is a command in typing mode"
        );
    }
    for (c, want) in [
        ('q', Action::Quit),
        ('n', Action::NewRun),
        ('a', Action::Artifacts),
        ('c', Action::Cancel),
        ('C', Action::Clone),
        ('o', Action::Open),
        ('d', Action::Download),
        ('p', Action::Message),
        ('s', Action::FreshSession),
        ('m', Action::Model),
        ('f', Action::Finish),
        ('j', Action::ScrollDown),
        ('k', Action::ScrollUp),
        ('g', Action::Top),
        ('G', Action::Bottom),
        // `r` is the offer to retry a blocked run; on any other run it opens the Runs list, as it always did.
        ('r', Action::Retry),
        ('i', Action::StartTyping),
    ] {
        assert_eq!(
            keys::lookup(Mode::Browse, &key(KeyCode::Char(c))),
            Some(want),
            "{c}"
        );
    }
    // Esc then the key, read by crossterm as Alt+key, is the key in browse mode; the shifted letter carries its case.
    assert_eq!(
        keys::lookup(Mode::Browse, &with(KeyCode::Char('c'), KeyModifiers::ALT)),
        Some(Action::Cancel)
    );
    assert_eq!(
        keys::lookup(Mode::Browse, &with(KeyCode::Char('G'), KeyModifiers::SHIFT)),
        Some(Action::Bottom)
    );
}

// ---- modes --------------------------------------------------------------------------------------------------------------------

#[test]
fn typing_cancel_in_the_box_cancels_nothing() {
    let mut app = typing();
    type_text(&mut app, "cancel the run, quit, finish");
    assert_eq!(app.screen(), Screen::Dashboard);
    assert!(screen(&app, 100, 30).contains("cancel the run, quit, finish"));
    assert_eq!(
        app.chat_text().as_deref(),
        Some("cancel the run, quit, finish")
    );
}

#[test]
fn esc_leaves_typing_and_then_the_old_hotkeys_work_as_before() {
    let mut app = typing();
    type_text(&mut app, "draft");
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(app.mode(), Mode::Browse);
    assert_eq!(app.screen(), Screen::Dashboard);
    let t = Instant::now();
    let asked = app.handle_key_at(key(KeyCode::Char('c')), t);
    assert!(asked.is_empty());
    // The second c counts 300 ms after the preview: a person's pace is well past it.
    let cancel = app.handle_key_at(key(KeyCode::Char('c')), t + Duration::from_millis(400));
    assert!(matches!(cancel.as_slice(), [Effect::Cancel { run_id }] if run_id == RUN));
    // The draft is still in the box.
    assert_eq!(app.chat_text().as_deref(), Some("draft"));
}

#[test]
fn esc_in_browse_mode_goes_back_to_the_runs_screen_as_today() {
    let mut app = dashboard();
    assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::LoadRuns]);
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn i_and_enter_return_to_typing() {
    let mut app = dashboard();
    assert!(ch(&mut app, 'i').is_empty());
    assert_eq!(app.mode(), Mode::Typing);
    press(&mut app, KeyCode::Esc);
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(app.mode(), Mode::Typing);
    // The `i` that started typing is not text.
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

#[test]
fn alt_and_a_hotkey_in_one_chunk_is_esc_then_the_key() {
    // Esc and `c` written in one chunk are read as Alt+c: the same result as two writes.
    let mut two = typing();
    press(&mut two, KeyCode::Esc);
    assert!(ch(&mut two, 'c').is_empty());
    let mut one = typing();
    let t = Instant::now();
    assert!(
        one.handle_key_at(with(KeyCode::Char('c'), KeyModifiers::ALT), t)
            .is_empty()
    );
    assert_eq!(one.mode(), Mode::Browse);
    assert_eq!(
        screen(&one, 100, 30).contains("press c again"),
        screen(&two, 100, 30).contains("press c again")
    );
    // In browse mode Alt-c is the letter c (the table's rule), so the second one confirms once the preview is 300 ms old.
    let soon = one.handle_key_at(
        with(KeyCode::Char('c'), KeyModifiers::ALT),
        t + Duration::from_millis(100),
    );
    assert!(soon.is_empty(), "inside 300 ms: {soon:?}");
    let cancel = one.handle_key_at(
        with(KeyCode::Char('c'), KeyModifiers::ALT),
        t + Duration::from_millis(500),
    );
    assert!(matches!(cancel.as_slice(), [Effect::Cancel { .. }]));
}

#[test]
fn alt_letters_the_editor_owns_stay_in_typing_mode() {
    // Alt-f is a word move and Alt-d a word delete in the editor: never Esc then finish or download.
    let mut app = typing();
    for c in ['f', 'd', 'n', 'p', 'b'] {
        assert!(
            app.handle_key(with(KeyCode::Char(c), KeyModifiers::ALT))
                .is_empty()
        );
        assert_eq!(app.mode(), Mode::Typing, "Alt-{c} left typing mode");
    }
}

#[test]
fn ctrl_c_clears_a_box_with_text_then_a_second_one_quits() {
    let mut app = typing();
    type_text(&mut app, "hello");
    let ctrl_c = with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.handle_key(ctrl_c).is_empty());
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert_eq!(app.handle_key(ctrl_c), vec![Effect::Quit]);
}

// ---- the box ------------------------------------------------------------------------------------------------------------------

#[test]
fn the_box_shows_the_text_with_the_caret_at_the_right_place() {
    let mut app = typing();
    type_text(&mut app, "fix the bug");
    for _ in 0..3 {
        press(&mut app, KeyCode::Left);
    }
    type_text(&mut app, "big ");
    let (text, (x, y)) = draw(&app, 100, 30);
    assert!(text.contains("fix the big bug"), "{text}");
    let row = text
        .lines()
        .position(|l| l.contains("fix the big bug"))
        .unwrap();
    assert_eq!(usize::from(y), row, "{text}");
    assert_eq!(x, 1 + "fix the big ".len() as u16, "{text}");
    press(&mut app, KeyCode::Home);
    type_text(&mut app, ">");
    press(&mut app, KeyCode::End);
    type_text(&mut app, "!");
    assert!(screen(&app, 100, 30).contains(">fix the big bug!"));
}

#[test]
fn enter_with_no_text_sends_nothing() {
    let mut app = typing();
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    type_text(&mut app, "   ");
    assert!(press(&mut app, KeyCode::Enter).is_empty());
}

#[test]
fn ctrl_j_shift_enter_and_backslash_enter_add_a_line() {
    let mut app = typing();
    type_text(&mut app, "one");
    assert!(
        app.handle_key(with(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .is_empty()
    );
    type_text(&mut app, "two");
    assert!(
        app.handle_key(with(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_empty()
    );
    type_text(&mut app, "three\\");
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    type_text(&mut app, "four");
    assert_eq!(app.chat_text().as_deref(), Some("one\ntwo\nthree\nfour"));
    let text = screen(&app, 100, 30);
    for want in ["one", "two", "three", "four"] {
        assert!(text.contains(want), "{text}");
    }
}

#[test]
fn the_box_grows_with_its_text_up_to_a_third_of_the_screen() {
    let mut app = typing();
    let rows_of = |app: &App| {
        screen(app, 100, 30)
            .lines()
            .filter(|l| l.contains("zzz"))
            .count()
    };
    type_text(&mut app, "zzz");
    assert_eq!(rows_of(&app), 1);
    for _ in 0..30 {
        app.handle_key(with(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_text(&mut app, "zzz");
    }
    // 30 rows: at most a third of the screen (10 rows) holds the text of the box, with the two border rows of the chat view around it.
    let shown = rows_of(&app);
    assert_eq!(shown, 10, "{shown} rows of text");
}

// ---- sending ------------------------------------------------------------------------------------------------------------------

thread_local! {
    static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn handled(_: &mut App, line: &str) -> Submit {
    SEEN.with(|s| s.borrow_mut().push(line.to_string()));
    Submit::Handled(vec![Effect::LoadRuns])
}

fn refused(_: &mut App, line: &str) -> Submit {
    SEEN.with(|s| s.borrow_mut().push(line.to_string()));
    Submit::Refused("no command cancle; closest: cancel".to_string())
}

fn passed(_: &mut App, line: &str) -> Submit {
    SEEN.with(|s| s.borrow_mut().push(line.to_string()));
    Submit::Pass
}

fn seen() -> Vec<String> {
    SEEN.with(|s| s.borrow().clone())
}

fn with_sink(sink: CommandSink) -> App {
    SEEN.with(|s| s.borrow_mut().clear());
    let mut app = typing();
    app.set_command_sink(sink);
    app
}

#[test]
fn enter_hands_the_trimmed_text_to_the_sink_and_a_handled_line_clears_the_box() {
    let mut app = with_sink(handled);
    type_text(&mut app, "  /runs  ");
    assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::LoadRuns]);
    assert_eq!(seen(), ["/runs"]);
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert_eq!(app.mode(), Mode::Typing, "typing goes on");
}

#[test]
fn a_refused_line_keeps_its_text_and_shows_the_reason() {
    let mut app = with_sink(refused);
    type_text(&mut app, "/cancle");
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("/cancle"));
    assert!(screen(&app, 100, 30).contains("closest: cancel"));
}

#[test]
fn a_line_the_sink_passes_on_goes_to_the_run_as_a_message_when_no_question_is_open() {
    let mut app = with_sink(passed);
    type_text(&mut app, "please add tests");
    let effects = press(&mut app, KeyCode::Enter);
    assert_eq!(seen(), ["please add tests"]);
    // Since the default route of a line on a working run is a message for after the turn (`tests/tui_send.rs` has the states).
    let sent: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Ext(ham::tui::ext::Job::SendMessage { request, .. }) => {
                Some(request.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["please add tests"], "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some(""));
}

// ---- the default route: answering an open question ----------------------------------------------------------------------------

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

fn waiting_in_typing_mode() -> App {
    let mut app = dashboard();
    let events = vec![
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
        question(),
    ];
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
    ch(&mut app, 'i');
    app
}

#[test]
fn with_a_question_open_enter_answers_it_in_the_same_session() {
    let mut app = waiting_in_typing_mode();
    type_text(&mut app, "use the other API");
    // Enter is bound to what the box showed: a question that opened less than 300 ms before it blocks it, so the key comes
    // 400 ms after the screen took the question in.
    let effects = app.handle_key_at(
        key(KeyCode::Enter),
        Instant::now() + Duration::from_millis(400),
    );
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
    assert_eq!(
        app.chat_text().as_deref(),
        Some(""),
        "the box is empty once the answer is on its way"
    );
}

// ---- paste and bursts ---------------------------------------------------------------------------------------------------------

#[test]
fn a_paste_into_the_box_is_text_and_never_sends() {
    let mut app = with_sink(handled);
    let effects = app.handle_paste("first\nsecond\n/runs\n".to_string());
    assert!(effects.is_empty());
    assert!(seen().is_empty(), "a paste reached the sink: {:?}", seen());
    // Three lines make a placeholder; the text that was pasted is what a send would read.
    assert_eq!(app.chat_text().as_deref(), Some("[Pasted 3 lines]"));
    assert_eq!(
        app.expanded_chat_text().as_deref(),
        Some("first\nsecond\n/runs\n")
    );
}

#[test]
fn a_paste_in_browse_mode_is_ignored_and_says_so() {
    let mut app = dashboard();
    assert!(app.handle_paste("cc".to_string()).is_empty());
    assert_eq!(app.chat_text().as_deref(), Some(""));
    assert!(screen(&app, 100, 30).contains("paste ignored"));
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn a_three_line_paste_that_arrives_as_keys_sends_nothing() {
    let mut app = with_sink(handled);
    let t = Instant::now();
    for (ms, c) in "one\rtwo\rthree\r".chars().enumerate() {
        let code = if c == '\r' {
            KeyCode::Enter
        } else {
            KeyCode::Char(c)
        };
        let effects = app.handle_key_at(key(code), at(t, ms as u64));
        assert!(effects.is_empty(), "{c:?} caused {effects:?}");
    }
    assert!(seen().is_empty(), "the burst sent {:?}", seen());
    assert_eq!(app.chat_text().as_deref(), Some("one\ntwo\nthree\n"));
}

#[test]
fn slash_cancel_and_two_enters_in_one_burst_send_no_command() {
    let mut app = with_sink(handled);
    let t = Instant::now();
    for (ms, c) in "/cancel\r\r".chars().enumerate() {
        let code = if c == '\r' {
            KeyCode::Enter
        } else {
            KeyCode::Char(c)
        };
        app.handle_key_at(key(code), at(t, ms as u64));
    }
    assert!(seen().is_empty(), "the burst sent {:?}", seen());
}

#[test]
fn an_enter_after_the_quiet_time_sends_what_the_burst_typed() {
    let mut app = with_sink(handled);
    let t = Instant::now();
    for (i, c) in "ab".chars().enumerate() {
        app.handle_key_at(key(KeyCode::Char(c)), at(t, i as u64));
    }
    // 120 ms later is still inside the 150 ms quiet time: a new line, not a send.
    app.handle_key_at(key(KeyCode::Enter), at(t, 120));
    assert!(seen().is_empty());
    // 500 ms after the last key a typed Enter sends.
    app.handle_key_at(key(KeyCode::Char('c')), at(t, 600));
    let effects = app.handle_key_at(key(KeyCode::Enter), at(t, 1200));
    assert_eq!(effects, vec![Effect::LoadRuns]);
    assert_eq!(seen(), ["ab\nc"]);
}

#[test]
fn enter_after_a_bracketed_paste_inside_150_ms_is_a_new_line() {
    let mut app = with_sink(handled);
    let t = Instant::now();
    app.handle_paste_at("pasted".to_string(), at(t, 0));
    app.handle_key_at(key(KeyCode::Enter), at(t, 50));
    assert!(seen().is_empty());
    assert_eq!(app.chat_text().as_deref(), Some("pasted\n"));
}

#[test]
fn the_second_c_of_a_burst_does_not_cancel_but_the_other_hotkeys_are_not_held_back() {
    // The live drivers press `f` twice in two writes, a few microseconds apart, and a bracketed paste cannot reach the hotkeys because it is
    // text; so the burst guard leaves the hotkeys alone. The one exception is the second `c` of `c c`:
    // the preview of the cancel is up and a confirming key within 300 ms of it is ignored, burst or not.
    let mut app = dashboard();
    let t = Instant::now();
    assert!(
        app.handle_key_at(key(KeyCode::Char('c')), at(t, 0))
            .is_empty()
    );
    let soon = app.handle_key_at(key(KeyCode::Char('c')), at(t, 1));
    assert!(soon.is_empty(), "a c 1 ms later cancelled: {soon:?}");
    let cancel = app.handle_key_at(key(KeyCode::Char('c')), at(t, 350));
    assert!(matches!(cancel.as_slice(), [Effect::Cancel { .. }]));
    // Any other hotkey right behind another key still acts: `a` opens the artifacts of the run.
    let mut app = dashboard();
    app.handle_key_at(key(KeyCode::Char('k')), at(t, 0));
    let effects = app.handle_key_at(key(KeyCode::Char('a')), at(t, 1));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadArtifacts { .. })),
        "{effects:?}"
    );
}

// ---- drafts -------------------------------------------------------------------------------------------------------------------

#[test]
fn each_run_keeps_its_own_draft_for_the_life_of_the_process() {
    let mut app = typing();
    type_text(&mut app, "draft for one");
    press(&mut app, KeyCode::Esc); // browse
    press(&mut app, KeyCode::Esc); // runs
    assert_eq!(app.screen(), Screen::Runs);
    app.open_run(RUN2);
    assert_eq!(
        app.chat_text().as_deref(),
        Some(""),
        "the other run starts empty"
    );
    ch(&mut app, 'i');
    type_text(&mut app, "draft for two");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    app.open_run(RUN);
    assert_eq!(app.chat_text().as_deref(), Some("draft for one"));
    app.open_run(RUN2);
    assert_eq!(app.chat_text().as_deref(), Some("draft for two"));
}

#[test]
fn quitting_with_a_draft_says_so_once() {
    let mut app = typing();
    type_text(&mut app, "unsent words");
    press(&mut app, KeyCode::Esc);
    assert!(ch(&mut app, 'q').is_empty(), "quit with an unsent draft");
    assert!(screen(&app, 100, 30).contains("unsent"));
    assert_eq!(ch(&mut app, 'q'), vec![Effect::Quit]);
}

// ---- the screens --------------------------------------------------------------------------------------------------------------

fn bottom(text: &str, rows: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(rows)..].join("\n")
}

#[test]
fn the_footer_and_box_in_both_modes_at_three_sizes() {
    for (w, h) in SIZES {
        let browse = dashboard();
        insta::assert_snapshot!(
            format!("keys_browse_{w}x{h}"),
            bottom(&screen(&browse, w, h), 5)
        );
        let mut typing = dashboard();
        ch(&mut typing, 'i');
        type_text(&mut typing, "fix the bug");
        insta::assert_snapshot!(
            format!("keys_typing_{w}x{h}"),
            bottom(&screen(&typing, w, h), 5)
        );
    }
}

#[test]
fn a_run_can_open_in_typing_mode() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_run_opens_typing(true);
    app.open_run(RUN);
    assert_eq!(app.mode(), Mode::Typing);
    type_text(&mut app, "cancel");
    assert_eq!(app.chat_text().as_deref(), Some("cancel"));
}

#[test]
fn the_composer_and_the_runs_screen_have_no_modes_and_keep_their_keys() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    assert_eq!(press(&mut app, KeyCode::Char('r')), vec![Effect::LoadRuns]);
    ch(&mut app, 'n');
    assert_eq!(app.screen(), Screen::Composer);
    type_text(&mut app, "qqq");
    assert_eq!(
        app.screen(),
        Screen::Composer,
        "letters in the composer are text"
    );
}
