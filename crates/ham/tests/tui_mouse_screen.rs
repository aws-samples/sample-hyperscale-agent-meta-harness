//! The mouse on the screens. Every draw fills the hit map with a region for each box, run row, fold,
//! key chip and scrolling pane, each naming the key that does the same; `App::handle_mouse_at` turns a crossterm mouse event into the
//! pointer's action and acts on it: a click on a box focuses it (the next typed character lands there), on a run row selects it (a second
//! click on the same run inside 500 ms opens it), on a fold toggles it, on a key chip presses that key; the wheel scrolls the pane
//! under the pointer by the step. Text selection and copy are the next step and are not tested here.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::output::{Chunk, CmdOutput, Stream};
use ham::tui::keys::Mode;
use ham::tui::mouse::{Target, WheelSettings};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::{EventsPage, RunDetail, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const RUN: &str = "01K6A0000000000000000000R1";
const R1: &str = "01M5A00000000000000000R001";
const R2: &str = "01M5A00000000000000000R002";
const R3: &str = "01M5A00000000000000000R003";
const R6: &str = "01M5A00000000000000000R006";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

// ---- helpers --------------------------------------------------------------------------------------------------------------------

fn text_of(app: &App, w: u16, h: u16) -> String {
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

/// The screen at 120x40, drawn (so the hit map is the one of this frame).
fn screen(app: &App) -> String {
    text_of(app, 120, 40)
}

/// The column and row where `needle` starts.
fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

fn row_text(text: &str, y: u16) -> &str {
    text.lines().nth(usize::from(y)).unwrap_or("")
}

fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    app.handle_mouse_at(
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        at,
    )
}

/// A left press and release at `(x, y)` at the instant `at`.
fn click(app: &mut App, x: u16, y: u16, at: Instant) -> Vec<Effect> {
    let mut effects = mouse(app, MouseEventKind::Down(MouseButton::Left), x, y, at);
    effects.extend(mouse(app, MouseEventKind::Up(MouseButton::Left), x, y, at));
    effects
}

fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> Vec<Effect> {
    key(app, KeyCode::Char(c))
}

// ---- the Runs fixtures ----------------------------------------------------------------------------------------------------------

fn run(id: &str, status: &str, target: Value) -> RunSummary {
    serde_json::from_value(
        json!({"runId": id, "status": status, "phase": "planning", "target": target,
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

fn repo(name: &str) -> Value {
    json!({"mode": "repo", "repo": format!("cc:{name}")})
}

fn six() -> Vec<RunSummary> {
    vec![
        run(R1, "complete", repo("calc-service")),
        run(R2, "active", repo("web-shop")),
        run(R3, "active", repo("web-shop")),
        run(
            "01M5A00000000000000000R004",
            "complete",
            repo("api-gateway"),
        ),
        run(
            "01M5A00000000000000000R005",
            "complete",
            json!({"mode": "s3-only"}),
        ),
        run(R6, "complete", repo("calc-service")),
    ]
}

fn runs_app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(six())));
    assert_eq!(app.screen(), Screen::Runs);
    app
}

fn composer_app() -> App {
    let mut app = runs_app();
    ch(&mut app, 'n');
    assert_eq!(app.screen(), Screen::Composer);
    app
}

// ---- the chat fixtures ----------------------------------------------------------------------------------------------------------

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

fn detail() -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": ARN},
        "objective": "Build a todo API with tests", "tokensUsed": 113000, "costUsd": 0.14
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

fn conversation(steps: u32) -> Vec<Envelope> {
    let mut e = vec![
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
    ];
    for i in 0..steps {
        e.push(env(
            100 + 3 * i,
            EventType::TurnProgress,
            Some("direct"),
            json!({"turn": 1, "kind": "message", "text": format!("Step {i:02} is done.")}),
        ));
        e.push(env(
            101 + 3 * i,
            EventType::ToolSummary,
            Some("direct"),
            json!({"toolCallId": format!("t{i}"), "title": format!("tool number {i:02}"), "kind": "execute", "status": "pending"}),
        ));
        e.push(env(
            102 + 3 * i,
            EventType::TurnProgress,
            Some("direct"),
            json!({"toolCallId": format!("t{i}"), "status": "completed"}),
        ));
    }
    e
}

fn chat_app(steps: u32) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(conversation(steps)));
    app.update(detail());
    assert_eq!(app.mode(), Mode::Browse);
    app
}

// ---- the hit map ----------------------------------------------------------------------------------------------------------------

#[test]
fn every_region_of_every_screen_names_the_key_that_does_the_same() {
    let mut apps: Vec<(&str, App)> = vec![
        ("runs", runs_app()),
        ("composer", composer_app()),
        ("run, browsing", chat_app(3)),
    ];
    let mut typing = chat_app(3);
    ch(&mut typing, 'i');
    assert_eq!(typing.mode(), Mode::Typing);
    apps.push(("run, typing", typing));
    for (name, app) in &apps {
        let text = screen(app);
        let map = app.hit_map();
        assert!(
            !map.regions().is_empty(),
            "{name}: no region at all\n{text}"
        );
        let unkeyed: Vec<_> = map
            .unkeyed()
            .iter()
            .map(|r| format!("{:?}", r.target))
            .collect();
        assert!(
            unkeyed.is_empty(),
            "{name}: regions without a key: {unkeyed:?}"
        );
    }
}

#[test]
fn a_click_between_a_resize_and_the_next_draw_hits_nothing() {
    let mut app = runs_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, R3);
    app.resized();
    assert!(app.hit_map().is_stale());
    click(&mut app, x + 4, y, Instant::now());
    // R1 is still the selected run: the marker is on its row.
    let text = screen(&app);
    assert!(row_text(&text, pos_of(&text, R1).1).contains('▶'), "{text}");
    // After the draw the same click selects R3.
    click(&mut app, x + 4, y, Instant::now());
    let text = screen(&app);
    assert!(row_text(&text, pos_of(&text, R3).1).contains('▶'), "{text}");
}

// ---- the composer ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_on_each_composer_box_focuses_it_and_the_next_character_lands_there() {
    let t0 = Instant::now();
    for title in [
        "Repository (cc:<name>)",
        "Base ref (optional)",
        "Options",
        "Prompt (Enter adds a line)",
    ] {
        let mut app = composer_app();
        let text = screen(&app);
        let (x, y) = pos_of(&text, title);
        click(&mut app, x + 3, y + 1, t0);
        assert!(ch(&mut app, 'Z').is_empty());
        let text = screen(&app);
        assert!(
            text.contains(&format!("▶ {title}")),
            "{title}: the box is not focused\n{text}"
        );
        assert!(
            row_text(&text, y + 1).contains('Z'),
            "{title}: the character did not land in the box\n{text}"
        );
    }
}

#[test]
fn a_click_on_the_target_box_focuses_it() {
    let mut app = composer_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "Target (");
    click(&mut app, x + 3, y + 1, Instant::now());
    assert!(screen(&app).contains("▶ Target"), "{}", screen(&app));
}

#[test]
fn a_click_on_a_model_row_focuses_the_list_and_picks_that_model() {
    let mut app = composer_app();
    let text = screen(&app);
    // The first selectable row that is not the chosen one.
    let (_, y) = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("( ) ") && l.contains(" ctx ") && !l.contains("pending"))
        .map(|(y, l)| (l.find("( )").unwrap() as u16, y as u16))
        .next()
        .expect("a model row to pick");
    click(&mut app, 8, y, Instant::now());
    let text = screen(&app);
    assert!(row_text(&text, y).contains("(•)"), "{text}");
    assert!(
        text.contains("▶ Model") || text.contains("› (•)") || text.contains("›(•)"),
        "{text}"
    );
}

/// The composer filled as a person fills it: a prompt, and the `--s3-only` target (no repository to name).
fn filled_composer() -> App {
    let mut app = composer_app();
    ch(&mut app, 'x');
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    app
}

#[test]
fn a_click_on_the_submit_line_starts_the_run_as_ctrl_s_does() {
    let mut by_key = filled_composer();
    let ctrl_s = by_key.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let [Effect::SubmitDirect { args: wanted, .. }] = ctrl_s.as_slice() else {
        panic!("Ctrl-S: {ctrl_s:?}")
    };
    let mut app = filled_composer();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "[ Submit ]");
    let effects = click(&mut app, x + 2, y, Instant::now());
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("a click on [ Submit ] did not start the run: {effects:?}\n{text}")
    };
    assert_eq!(args, wanted, "the click sends what Ctrl-S sends");
    // A double click: the second press arrives before the form was drawn again, with the old hit map, and starts nothing more.
    let double = click(&mut app, x + 2, y, Instant::now());
    assert!(double.is_empty(), "{double:?}");
    // The line says the run is starting, and a click on the same place after the redraw starts nothing either.
    let text = screen(&app);
    assert!(text.contains("submitting"), "{text}");
    let again = click(&mut app, x + 2, y, Instant::now());
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn a_click_on_the_submit_line_with_no_prompt_says_a_prompt_is_required() {
    let mut app = composer_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "[ Submit ]");
    let effects = click(&mut app, x + 2, y, Instant::now());
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("a prompt is required"),
        "{}",
        screen(&app)
    );
}

#[test]
fn the_submit_region_names_ctrl_s_and_covers_only_its_words() {
    let app = filled_composer();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "[ Submit ]");
    let map = app.hit_map();
    let region = map
        .target_at(x + 2, y)
        .unwrap_or_else(|| panic!("no region on the Submit line\n{text}"));
    assert_eq!(region.key, "Ctrl-S");
    // The blank part of the line to the right of its words is no button.
    assert!(
        map.target_at(110, y).is_none_or(|r| r.key != "Ctrl-S"),
        "{text}"
    );
}

#[test]
fn the_keyboard_path_is_unchanged_without_a_mouse() {
    let mut app = composer_app();
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Tab);
    assert!(screen(&app).contains("▶ Repository"));
}

// ---- the Runs list --------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_on_a_run_selects_it_and_a_second_click_inside_500_ms_opens_it() {
    let mut app = runs_app();
    let t0 = Instant::now();
    let text = screen(&app);
    let (x, y) = pos_of(&text, R3);
    assert!(click(&mut app, x + 4, y, t0).is_empty());
    let text = screen(&app);
    assert!(row_text(&text, y).contains('▶'), "{text}");
    let effects = click(&mut app, x + 4, y, t0 + Duration::from_millis(300));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R3)),
        "{effects:?}"
    );
    assert_eq!(app.screen(), Screen::Dashboard);
}

#[test]
fn a_second_click_after_500_ms_only_selects_again() {
    let mut app = runs_app();
    let t0 = Instant::now();
    let text = screen(&app);
    let (x, y) = pos_of(&text, R3);
    click(&mut app, x + 4, y, t0);
    let effects = click(&mut app, x + 4, y, t0 + Duration::from_millis(900));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.screen(), Screen::Runs);
}

#[test]
fn a_click_on_another_run_inside_the_window_selects_that_run_and_does_not_open() {
    let mut app = runs_app();
    let t0 = Instant::now();
    let text = screen(&app);
    let (x, y2) = pos_of(&text, R2);
    let (_, y3) = pos_of(&text, R3);
    click(&mut app, x + 4, y2, t0);
    let effects = click(&mut app, x + 4, y3, t0 + Duration::from_millis(100));
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(row_text(&text, y3).contains('▶'), "{text}");
}

#[test]
fn clicks_follow_the_filtered_list() {
    let mut app = runs_app();
    ch(&mut app, '/');
    for c in "calc".chars() {
        ch(&mut app, c);
    }
    key(&mut app, KeyCode::Enter);
    let t0 = Instant::now();
    let text = screen(&app);
    // Two runs are shown: R1 then R6. A double click on the second opens R6.
    let (x, y6) = pos_of(&text, R6);
    click(&mut app, x + 4, y6, t0);
    let effects = click(&mut app, x + 4, y6, t0 + Duration::from_millis(200));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Poll { run_id, .. } if run_id == R6)),
        "{effects:?}"
    );
}

#[test]
fn a_click_below_the_last_run_does_nothing() {
    let mut app = runs_app();
    let t0 = Instant::now();
    let text = screen(&app);
    let (x, y) = pos_of(&text, R6);
    let effects = click(&mut app, x + 4, y + 3, t0);
    assert!(effects.is_empty());
    let text = screen(&app);
    assert!(row_text(&text, pos_of(&text, R1).1).contains('▶'), "{text}");
}

// The Runs list scrolls (package FIX-RUNS-SCROLL, 2026-10-21): a notch carries the lines of the wheel step (3 by default), and the Runs selection moves by them. It moved one
// run a notch before, which a list of hundreds of runs made too slow to use; `tui_runs_scroll.rs` holds the tests of the scrolled table.
const R4: &str = "01M5A00000000000000000R004";

#[test]
fn the_wheel_moves_the_selection_by_the_lines_of_a_notch() {
    let mut app = runs_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, R2);
    let t0 = Instant::now();
    mouse(&mut app, MouseEventKind::ScrollDown, x + 4, y, t0);
    let text = screen(&app);
    assert!(row_text(&text, pos_of(&text, R4).1).contains('▶'), "{text}");
    mouse(
        &mut app,
        MouseEventKind::ScrollUp,
        x + 4,
        y,
        t0 + Duration::from_millis(400),
    );
    let text = screen(&app);
    assert!(row_text(&text, pos_of(&text, R1).1).contains('▶'), "{text}");
}

// ---- key chips --------------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_on_a_key_chip_of_the_runs_screen_presses_that_key() {
    let mut app = runs_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "n new run");
    assert!(click(&mut app, x + 2, y, Instant::now()).is_empty());
    assert_eq!(app.screen(), Screen::Composer);
    // The refresh chip asks for the list again.
    let mut app = runs_app();
    let text = screen(&app);
    let (x, y) = pos_of(&text, "r refresh");
    let effects = click(&mut app, x + 2, y, Instant::now());
    assert!(
        effects.iter().any(|e| matches!(e, Effect::LoadRuns)),
        "{effects:?}"
    );
}

#[test]
fn a_click_on_a_key_chip_of_the_run_screen_presses_that_key() {
    let mut app = chat_app(3);
    let text = screen(&app);
    let (x, y) = pos_of(&text, "a artifacts");
    let effects = click(&mut app, x + 2, y, Instant::now());
    assert_eq!(app.screen(), Screen::Artifacts, "{effects:?}");
}

#[test]
fn the_esc_chip_of_the_typing_footer_leaves_typing() {
    let mut app = chat_app(3);
    ch(&mut app, 'i');
    assert_eq!(app.mode(), Mode::Typing);
    let text = screen(&app);
    let (x, y) = pos_of(&text, "Esc browse");
    click(&mut app, x + 1, y, Instant::now());
    assert_eq!(app.mode(), Mode::Browse);
}

// ---- the run screen ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_click_in_the_message_box_starts_typing_there() {
    let mut app = chat_app(3);
    let text = screen(&app);
    let (x, y) = pos_of(&text, "Message to Cline");
    assert_eq!(app.mode(), Mode::Browse);
    click(&mut app, x + 4, y + 1, Instant::now());
    assert_eq!(app.mode(), Mode::Typing);
    ch(&mut app, 'Z');
    assert_eq!(app.chat_text().as_deref(), Some("Z"));
}

#[test]
fn a_click_on_the_head_of_a_folded_card_opens_it_and_another_closes_it() {
    let mut app = chat_app(3);
    let t0 = Instant::now();
    let text = screen(&app);
    assert!(!text.contains("AGENT"), "{text}");
    let (x, y) = pos_of(&text, "agents 1");
    click(&mut app, x + 1, y, t0);
    let text = screen(&app);
    assert!(text.contains("AGENT"), "the card did not open:\n{text}");
    let (x, y) = pos_of(&text, "agents 1");
    click(&mut app, x + 1, y, t0 + Duration::from_millis(900));
    assert!(!screen(&app).contains("AGENT"));
}

#[test]
fn a_click_on_ordinary_conversation_text_changes_nothing() {
    let mut app = chat_app(3);
    let before = screen(&app);
    let (x, y) = pos_of(&before, "Step 00");
    click(&mut app, x + 2, y, Instant::now());
    assert_eq!(screen(&app), before);
    assert_eq!(app.mode(), Mode::Browse);
}

#[test]
fn the_wheel_scrolls_the_conversation_three_lines_a_notch() {
    let mut app = chat_app(30);
    let before = screen(&app);
    let (x, y_before) = pos_of(&before, "Step 20 is done.");
    let t0 = Instant::now();
    mouse(&mut app, MouseEventKind::ScrollUp, x + 2, y_before, t0);
    let after = screen(&app);
    let (_, y_after) = pos_of(&after, "Step 20 is done.");
    assert_eq!(y_after, y_before + 3, "{after}");
}

#[test]
fn the_wheel_step_is_the_setting() {
    let mut app = chat_app(30);
    app.set_wheel(WheelSettings::new(5));
    let before = screen(&app);
    let (x, y_before) = pos_of(&before, "Step 20 is done.");
    mouse(
        &mut app,
        MouseEventKind::ScrollUp,
        x + 2,
        y_before,
        Instant::now(),
    );
    let after = screen(&app);
    let (_, y_after) = pos_of(&after, "Step 20 is done.");
    assert_eq!(y_after, y_before + 5, "{after}");
}

#[test]
fn a_result_panel_shields_what_lies_under_it() {
    let mut app = chat_app(3);
    ch(&mut app, 'i');
    for c in "/runs".chars() {
        ch(&mut app, c);
    }
    // The line is a command: Enter shows a preview or runs it; here the result panel is put on screen by the answer.
    app.update(Msg::CommandDone {
        argv: vec!["runs".into()],
        out: CmdOutput {
            chunks: vec![Chunk {
                stream: Stream::Out,
                text: "one result line\n".into(),
            }],
            code: 0,
            bearer: None,
        },
    });
    let text = screen(&app);
    assert!(text.contains("one result line"), "{text}");
    let map = app.hit_map();
    assert!(
        map.regions().iter().any(|r| r.target == Target::Block),
        "no region shields the panel"
    );
    let unkeyed = map.unkeyed();
    assert!(unkeyed.is_empty(), "{unkeyed:?}");
}
