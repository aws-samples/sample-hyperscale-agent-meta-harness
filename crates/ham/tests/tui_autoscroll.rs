//! Autoscroll for a selection drag that is held outside the pane.
//! A terminal reports a drag only when the pointer moves, so a pointer held still above or below the conversation sends
//! nothing, and the conversation used to stop after the one report that crossed the edge. `Pointer::tick` repeats the scroll every
//! `AUTOSCROLL_EVERY` (60 ms); `App::autoscroll_at` feeds it through the mouse path, `App::autoscroll_due` tells the event loop when to wake,
//! and the loop (`tui::event_loop_timed`) sleeps until then only while a drag is held outside. The repeat stops when the button is released,
//! on a key, on a resize, when the pointer is back inside the pane (and starts again when it leaves); the selection follows the scroll.
//! The clock is injected everywhere except in the one test that runs the real loop, so these tests count and compare.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::keys::Mode;
use ham::tui::mouse::{
    AUTOSCROLL_EVERY, Action, DEFAULT_WHEEL_STEP, HitMap, PaneId, Pointer, Target, WheelSettings,
};
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const CHAT: PaneId = PaneId(1);
const FRAME: Rect = Rect {
    x: 0,
    y: 0,
    width: 100,
    height: 30,
};

// ---- the pure pointer -------------------------------------------------------------------------------------------------------------

fn ev(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn down(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Down(MouseButton::Left), x, y)
}
fn up(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Up(MouseButton::Left), x, y)
}
fn drag(x: u16, y: u16) -> MouseEvent {
    ev(MouseEventKind::Drag(MouseButton::Left), x, y)
}

/// A chat pane over rows 10 to 24 and nothing else.
fn fill(m: &mut HitMap) {
    m.begin(FRAME);
    m.push(Rect::new(0, 10, 100, 15), Target::Text(CHAT), "y");
}

fn pane() -> HitMap {
    let mut m = HitMap::new();
    fill(&mut m);
    m
}

fn pointer() -> Pointer {
    Pointer::new(WheelSettings::default())
}

fn scrolls(actions: &[Action]) -> Vec<i32> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Autoscroll { lines, .. } => Some(*lines),
            _ => None,
        })
        .collect()
}

/// A pointer that holds still above the pane after one drag report; the time of that report is returned.
fn held_above(m: &HitMap) -> (Pointer, Instant) {
    let t = Instant::now();
    let mut p = pointer();
    p.handle(m, down(10, 14), t);
    let first = p.handle(m, drag(10, 4), t);
    assert_eq!(
        scrolls(&first),
        vec![-i32::from(DEFAULT_WHEEL_STEP)],
        "TRIGGER: the report that crossed the edge scrolled once"
    );
    (p, t)
}

#[test]
fn nothing_is_due_while_no_drag_is_held() {
    let m = pane();
    let mut p = pointer();
    let t = Instant::now();
    assert_eq!(p.autoscroll_due(&m, t), None, "nothing pressed");
    p.handle(&m, down(10, 14), t);
    assert_eq!(p.autoscroll_due(&m, t), None, "pressed, not dragged");
    p.handle(&m, drag(20, 14), t);
    assert_eq!(p.autoscroll_due(&m, t), None, "dragged inside the pane");
}

#[test]
fn a_held_drag_outside_is_due_one_interval_after_its_last_scroll_and_then_every_interval() {
    let m = pane();
    let (mut p, t) = held_above(&m);
    assert_eq!(p.autoscroll_due(&m, t), Some(t + AUTOSCROLL_EVERY));
    // The loop wakes at the deadline, not before: a tick that is too early scrolls nothing and moves no deadline.
    assert!(scrolls(&p.tick(&m, t + AUTOSCROLL_EVERY / 2)).is_empty());
    assert_eq!(p.autoscroll_due(&m, t), Some(t + AUTOSCROLL_EVERY));
    let at = t + AUTOSCROLL_EVERY;
    assert_eq!(
        scrolls(&p.tick(&m, at)),
        vec![-i32::from(DEFAULT_WHEEL_STEP)]
    );
    assert_eq!(p.autoscroll_due(&m, at), Some(at + AUTOSCROLL_EVERY));
    // A wake that comes late scrolls once, not once per missed interval.
    let late = at + AUTOSCROLL_EVERY * 5;
    assert_eq!(
        scrolls(&p.tick(&m, late)),
        vec![-i32::from(DEFAULT_WHEEL_STEP)]
    );
    assert_eq!(p.autoscroll_due(&m, late), Some(late + AUTOSCROLL_EVERY));
}

#[test]
fn below_the_pane_the_repeat_scrolls_the_other_way() {
    let m = pane();
    let t = Instant::now();
    let mut p = pointer();
    p.handle(&m, down(10, 14), t);
    p.handle(&m, drag(10, 29), t);
    assert_eq!(
        scrolls(&p.tick(&m, t + AUTOSCROLL_EVERY)),
        vec![i32::from(DEFAULT_WHEEL_STEP)]
    );
}

#[test]
fn the_repeat_stops_when_the_button_comes_up() {
    let m = pane();
    let (mut p, t) = held_above(&m);
    assert_eq!(
        p.autoscroll_due(&m, t),
        Some(t + AUTOSCROLL_EVERY),
        "TRIGGER: the repeat is pending before the release"
    );
    p.handle(&m, up(10, 4), t + AUTOSCROLL_EVERY / 2);
    assert_eq!(p.autoscroll_due(&m, t + AUTOSCROLL_EVERY), None);
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY * 3), vec![]);
}

#[test]
fn the_repeat_stops_when_the_pointer_is_back_inside_the_pane_and_starts_again_when_it_leaves() {
    let m = pane();
    let (mut p, t) = held_above(&m);
    let t1 = t + AUTOSCROLL_EVERY;
    p.handle(&m, drag(10, 12), t1);
    assert_eq!(p.autoscroll_due(&m, t1), None, "inside");
    assert_eq!(p.tick(&m, t1 + AUTOSCROLL_EVERY * 4), vec![]);
    let t2 = t1 + AUTOSCROLL_EVERY * 5;
    p.handle(&m, drag(10, 29), t2);
    assert_eq!(p.autoscroll_due(&m, t2), Some(t2 + AUTOSCROLL_EVERY));
    assert_eq!(
        scrolls(&p.tick(&m, t2 + AUTOSCROLL_EVERY)),
        vec![i32::from(DEFAULT_WHEEL_STEP)]
    );
}

#[test]
fn a_key_stops_the_repeat_and_the_next_drag_report_starts_it_again() {
    let m = pane();
    let (mut p, t) = held_above(&m);
    p.note_key();
    assert_eq!(p.autoscroll_due(&m, t + AUTOSCROLL_EVERY), None);
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY * 3), vec![]);
    // The pointer moves again, still above the pane: that report scrolls, and the repeat is back.
    let t2 = t + AUTOSCROLL_EVERY * 4;
    let a = p.handle(&m, drag(11, 3), t2);
    assert_eq!(scrolls(&a), vec![-i32::from(DEFAULT_WHEEL_STEP)]);
    assert_eq!(p.autoscroll_due(&m, t2), Some(t2 + AUTOSCROLL_EVERY));
}

#[test]
fn a_key_with_no_drag_held_changes_nothing() {
    let m = pane();
    let mut p = pointer();
    p.note_key();
    let t = Instant::now();
    p.handle(&m, down(10, 14), t);
    p.note_key();
    p.handle(&m, drag(10, 4), t);
    assert_eq!(
        p.autoscroll_due(&m, t),
        Some(t + AUTOSCROLL_EVERY),
        "a key before the drag does not stop the drag that follows"
    );
}

#[test]
fn a_resize_stops_the_repeat_even_after_the_screen_is_drawn_again() {
    let mut m = pane();
    let (mut p, t) = held_above(&m);
    assert_eq!(
        p.autoscroll_due(&m, t),
        Some(t + AUTOSCROLL_EVERY),
        "TRIGGER: the repeat is pending before the resize"
    );
    m.resized();
    assert_eq!(p.autoscroll_due(&m, t + AUTOSCROLL_EVERY), None, "stale");
    fill(&mut m);
    assert_eq!(
        p.autoscroll_due(&m, t + AUTOSCROLL_EVERY),
        None,
        "drawn again: the drag began before the resize, so it is over"
    );
    // The half-made selection is dropped once, and nothing repeats after it.
    assert_eq!(
        p.tick(&m, t + AUTOSCROLL_EVERY * 2),
        vec![Action::ClearSelection]
    );
    assert_eq!(p.tick(&m, t + AUTOSCROLL_EVERY * 4), vec![]);
}

#[test]
fn a_screen_without_the_pane_stops_the_repeat() {
    let (mut p, t) = held_above(&pane());
    let mut other = HitMap::new();
    other.begin(FRAME);
    other.push(Rect::new(0, 0, 100, 5), Target::Scroll(CHAT), "↑↓");
    assert_eq!(p.autoscroll_due(&other, t + AUTOSCROLL_EVERY), None);
    assert_eq!(p.tick(&other, t + AUTOSCROLL_EVERY), vec![]);
}

#[test]
fn the_edge_is_the_edge_of_the_pane_as_drawn_now() {
    let t = Instant::now();
    let mut p = pointer();
    let m = pane();
    p.handle(&m, down(10, 14), t);
    // The pointer rests on row 24, the last row of the pane: inside, so nothing repeats.
    p.handle(&m, drag(10, 24), t);
    assert_eq!(p.autoscroll_due(&m, t), None);
    // The screen is drawn again with a taller box under the conversation: the pane now ends at row 22 and the pointer is below it.
    let mut shorter = HitMap::new();
    shorter.begin(FRAME);
    shorter.push(Rect::new(0, 10, 100, 13), Target::Text(CHAT), "y");
    assert_eq!(
        p.autoscroll_due(&shorter, t),
        Some(t),
        "due at once: it never scrolled"
    );
    let a = p.tick(&shorter, t);
    assert_eq!(scrolls(&a), vec![i32::from(DEFAULT_WHEEL_STEP)]);
    assert!(
        a.contains(&Action::SelectionExtend {
            pane: CHAT,
            x: 10,
            y: 22
        }),
        "the selection end is clamped into the pane as drawn now: {a:?}"
    );
}

#[derive(Clone, Debug)]
enum Op {
    Down(u16, u16),
    Drag(u16, u16),
    Up(u16, u16),
    Key,
    Resize,
    Draw,
    Wait(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..100u16, 0..30u16).prop_map(|(x, y)| Op::Down(x, y)),
        (0..100u16, 0..30u16).prop_map(|(x, y)| Op::Drag(x, y)),
        (0..100u16, 0..30u16).prop_map(|(x, y)| Op::Up(x, y)),
        Just(Op::Key),
        Just(Op::Resize),
        Just(Op::Draw),
        (0..200u64).prop_map(Op::Wait),
    ]
}

proptest! {
    /// The loop sleeps until `autoscroll_due` and then calls `tick`: if the two ever disagreed, the loop would either never wake for a held
    /// drag or wake in a tight loop for nothing. At every point of any sequence of events, a deadline that has passed means the tick scrolls,
    /// and no deadline means it does not.
    #[test]
    fn the_deadline_and_the_tick_always_agree(ops in prop::collection::vec(op(), 0..60)) {
        let mut m = pane();
        let mut p = pointer();
        let mut now = Instant::now();
        for o in ops {
            match o {
                Op::Down(x, y) => { p.handle(&m, down(x, y), now); }
                Op::Drag(x, y) => { p.handle(&m, drag(x, y), now); }
                Op::Up(x, y) => { p.handle(&m, up(x, y), now); }
                Op::Key => p.note_key(),
                Op::Resize => m.resized(),
                Op::Draw => fill(&mut m),
                Op::Wait(ms) => now += Duration::from_millis(ms),
            }
            let due = p.autoscroll_due(&m, now);
            let ready = due.is_some_and(|d| d <= now);
            let mut probe = p.clone();
            let scrolled = !scrolls(&probe.tick(&m, now)).is_empty();
            prop_assert_eq!(ready, scrolled, "due {:?} at {:?}", due, now);
        }
    }
}

// ---- the screen ---------------------------------------------------------------------------------------------------------------------

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn text_of(buf: &Buffer) -> String {
    let (w, h) = (buf.area.width, buf.area.height);
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

fn screen(app: &App) -> String {
    text_of(&buffer(app, 100, 30))
}

fn pos_of(text: &str, needle: &str) -> (u16, u16) {
    for (y, line) in text.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} is not on the screen:\n{text}");
}

/// The numbers of the `step NNN` rows on the screen, top to bottom.
fn steps_on(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| {
            let at = l.find("step ")?;
            l[at + 5..].get(..3)?.parse().ok()
        })
        .collect()
}

/// The first step shown: the top of the conversation.
fn top_step(app: &App) -> u32 {
    *steps_on(&screen(app)).first().unwrap()
}

fn reversed_rows(buf: &Buffer) -> Vec<u16> {
    let mut rows = Vec::new();
    for y in 0..buf.area.height {
        if (9..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::REVERSED)) {
            rows.push(y);
        }
    }
    rows
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

fn tool(n: u32, title: &str) -> Envelope {
    env(
        n,
        EventType::ToolSummary,
        Some("direct"),
        json!({"toolCallId": format!("t{n}"), "title": title, "kind": "execute", "status": "completed"}),
    )
}

fn many_steps(n: u32) -> Vec<Envelope> {
    (0..n)
        .map(|i| tool(100 + i, &format!("step {i:03}")))
        .collect()
}

fn on_screen(extra: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(1_792_000_000);
    app.open_run(RUN);
    let mut events = vec![
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
    events.extend(extra);
    app.update(pages(events));
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(d)));
    assert_eq!(app.mode(), Mode::Browse);
    app
}

/// The mouse of a person with a clock the test moves.
struct Mouse {
    base: Instant,
    ms: u64,
}

impl Mouse {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 0,
        }
    }

    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.ms)
    }

    fn wait(&mut self, d: Duration) {
        self.ms += u64::try_from(d.as_millis()).unwrap();
    }

    fn event(&mut self, app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Vec<Effect> {
        self.ms += 30;
        app.handle_mouse_at(ev(kind, x, y), self.now())
    }

    fn press(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn drag(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Drag(MouseButton::Left), x, y)
    }

    fn release(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        self.event(app, MouseEventKind::Up(MouseButton::Left), x, y)
    }

    /// One interval of holding still, then the loop's wake-up.
    fn hold(&mut self, app: &mut App) -> Vec<Effect> {
        self.wait(AUTOSCROLL_EVERY);
        app.autoscroll_at(self.now())
    }
}

fn copies(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Copy(p) => Some(p.text().to_string()),
            _ => None,
        })
        .collect()
}

/// A person who has pressed on the last row of 120 steps and dragged to the top row of the screen, above the conversation. The one
/// report that crossed the edge has scrolled it once.
fn held_at_the_top() -> (App, Mouse, (u16, u16)) {
    let mut app = on_screen(many_steps(120));
    let text = screen(&app);
    let (x_last, y_last) = pos_of(&text, "step 119");
    let before = top_step(&app);
    let mut m = Mouse::new();
    m.press(&mut app, x_last + 7, y_last);
    m.drag(&mut app, x_last, 0);
    assert!(
        top_step(&app) < before,
        "TRIGGER: the report above the pane scrolled the conversation once ({} -> {})",
        before,
        top_step(&app)
    );
    (app, m, (x_last, y_last))
}

#[test]
fn a_pointer_held_still_above_the_pane_keeps_scrolling_the_conversation_every_interval() {
    let (mut app, mut m, _) = held_at_the_top();
    let mut tops = vec![top_step(&app)];
    for _ in 0..8 {
        m.hold(&mut app);
        tops.push(top_step(&app));
    }
    let deltas: Vec<u32> = tops.windows(2).map(|w| w[0].saturating_sub(w[1])).collect();
    assert!(
        deltas.iter().all(|d| *d > 0),
        "every wake scrolls: {tops:?}"
    );
    assert!(
        deltas.windows(2).all(|w| w[0] == w[1]),
        "the same distance every time: {tops:?}"
    );
}

#[test]
fn a_wake_before_the_interval_scrolls_nothing() {
    let (mut app, mut m, _) = held_at_the_top();
    let top = top_step(&app);
    m.wait(AUTOSCROLL_EVERY / 2);
    assert!(app.autoscroll_at(m.now()).is_empty());
    assert_eq!(top_step(&app), top, "too soon");
    assert!(app.autoscroll_due(m.now()).is_some_and(|d| d > m.now()));
}

#[test]
fn the_selection_end_follows_the_scroll_and_the_release_copies_every_row_it_passed() {
    let (mut app, mut m, (x_last, y_last)) = held_at_the_top();
    let (_, y_first) = pos_of(&screen(&app), "step ");
    for _ in 0..8 {
        m.hold(&mut app);
        // The end the pointer holds is on the first row of the conversation, so every row from there down is selected.
        let rows = reversed_rows(&buffer(&app, 100, 30));
        let want: Vec<u16> = (y_first..=y_last).collect();
        assert!(
            want.iter().all(|y| rows.contains(y)),
            "rows {y_first}..={y_last} are reversed, got {rows:?}\n{}",
            screen(&app)
        );
    }
    let top = top_step(&app);
    let got = copies(&m.release(&mut app, x_last, 0));
    assert_eq!(got.len(), 1, "one copy for one selection");
    assert!(
        got[0].starts_with(&format!("step {top:03}")) && got[0].ends_with("step 119"),
        "the copy runs from the top row now shown to the row pressed: {:?}",
        got[0]
    );
    let steps = got[0].lines().filter(|l| l.starts_with("step ")).count();
    assert_eq!(
        steps as u32,
        119 - top + 1,
        "every row in between is in the copy"
    );
}

#[test]
fn releasing_the_button_stops_the_repeat() {
    let (mut app, mut m, (x_last, _)) = held_at_the_top();
    let before = top_step(&app);
    m.hold(&mut app);
    assert!(top_step(&app) < before, "TRIGGER: the repeat is running");
    m.release(&mut app, x_last, 0);
    let top = top_step(&app);
    for _ in 0..3 {
        assert!(m.hold(&mut app).is_empty());
    }
    assert_eq!(top_step(&app), top, "the conversation stays where it is");
    assert_eq!(app.autoscroll_due(m.now()), None);
}

#[test]
fn a_key_stops_the_repeat_and_moving_the_pointer_again_starts_it() {
    let (mut app, mut m, (x_last, _)) = held_at_the_top();
    let before = top_step(&app);
    m.hold(&mut app);
    assert!(top_step(&app) < before, "TRIGGER: the repeat is running");
    // A key that does nothing in browse mode.
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
    let top = top_step(&app);
    for _ in 0..3 {
        m.hold(&mut app);
    }
    assert_eq!(top_step(&app), top, "no scroll after the key");
    assert_eq!(app.autoscroll_due(m.now()), None);
    // The pointer moves one cell, still above the pane.
    m.drag(&mut app, x_last + 1, 0);
    let moved = top_step(&app);
    assert!(moved < top, "the report scrolls");
    m.hold(&mut app);
    assert!(top_step(&app) < moved, "and the repeat is back");
}

#[test]
fn a_resize_stops_the_repeat() {
    let (mut app, mut m, _) = held_at_the_top();
    let before = top_step(&app);
    m.hold(&mut app);
    assert!(top_step(&app) < before, "TRIGGER: the repeat is running");
    app.resized();
    // Drawn again at the same size: the map is fresh, but the drag began before the resize.
    let after = top_step(&app);
    for _ in 0..3 {
        m.hold(&mut app);
    }
    assert_eq!(top_step(&app), after, "no scroll after the resize");
    assert_eq!(app.autoscroll_due(m.now()), None);
}

#[test]
fn moving_back_into_the_pane_stops_the_repeat_and_leaving_again_starts_it() {
    let (mut app, mut m, (x_last, y_last)) = held_at_the_top();
    m.hold(&mut app);
    m.drag(&mut app, x_last, y_last - 2);
    let inside = top_step(&app);
    for _ in 0..3 {
        assert!(m.hold(&mut app).is_empty());
    }
    assert_eq!(top_step(&app), inside, "inside the pane nothing repeats");
    assert_eq!(app.autoscroll_due(m.now()), None);
    m.drag(&mut app, x_last, 0);
    let out = top_step(&app);
    m.hold(&mut app);
    assert!(top_step(&app) < out, "outside again: it repeats");
}

#[test]
fn below_the_pane_the_conversation_scrolls_toward_the_end_until_it_gets_there() {
    let mut app = on_screen(many_steps(120));
    let text = screen(&app);
    let (x, y_first) = pos_of(&text, "step ");
    let mut m = Mouse::new();
    // Back up the conversation by wheel notches, then press on its first row and drag to the bottom row of the screen.
    for _ in 0..15 {
        app.handle_wheel(false);
    }
    let high = top_step(&app);
    assert!(
        high < 60,
        "TRIGGER: the conversation is scrolled up ({high})"
    );
    m.press(&mut app, x, y_first);
    m.drag(&mut app, x, 29);
    let mut tops = vec![top_step(&app)];
    for _ in 0..6 {
        m.hold(&mut app);
        tops.push(top_step(&app));
    }
    assert!(
        tops.windows(2).all(|w| w[1] > w[0]),
        "it moves toward newer rows on every wake: {tops:?}"
    );
}

#[test]
fn the_one_second_timer_does_not_scroll_a_held_drag() {
    let (mut app, mut m, _) = held_at_the_top();
    let top = top_step(&app);
    m.wait(Duration::from_secs(1));
    app.tick();
    assert_eq!(
        top_step(&app),
        top,
        "the repeat belongs to the fast wake-up, not to the 1 s poll timer"
    );
}

// ---- the real loop --------------------------------------------------------------------------------------------------------------------

/// The event loop wakes by itself while a drag is held outside the pane: a press, one drag report above the pane, then nothing is sent for
/// a second and a half. The conversation has moved on many times by then. The 1 s timer is switched off (an hour), so the movement comes from
/// the loop's own wake-up for the held drag. `q` ends the run in browse mode; the button was never released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_event_loop_scrolls_a_held_drag_on_its_own_clock() {
    let fake = Fake::start().await;
    let api = Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let app = on_screen(many_steps(200));
    let text = screen(&app);
    let (x_last, y_last) = pos_of(&text, "step 199");
    let before = top_step(&app);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let mouse = |kind, x, y| Event::Mouse(ev(kind, x, y));
    tx.send(mouse(
        MouseEventKind::Down(MouseButton::Left),
        x_last + 7,
        y_last,
    ))
    .unwrap();
    tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), x_last, 0))
        .unwrap();
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let dir = support::scratch("tui-autoscroll-loop");
    let hold_then_quit = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    };
    let (result, ()) = tokio::join!(
        ham::tui::event_loop(
            &mut term,
            api,
            dir,
            app,
            vec![],
            rx,
            Duration::from_secs(3600),
        ),
        hold_then_quit
    );
    result.unwrap();
    let after = *steps_on(&text_of(term.backend().buffer())).first().unwrap();
    let rows = before - after;
    // The one drag report scrolls 3 rows; each wake after it scrolls 3 more. 1.5 s holds about 25 wakes; 6 is far below that and far above
    // the one a loop without the wake-up would have made.
    assert!(
        rows >= 3 * 6,
        "the loop scrolled {rows} rows in 1.5 s of holding still (from step {before} to step {after})"
    );
}
