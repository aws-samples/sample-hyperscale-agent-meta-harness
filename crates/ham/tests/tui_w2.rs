//! The run dashboard and the Artifacts screen: insta
//! snapshots at 80x24, 120x40 and 200x60 of an empty run, 1, 500 and 5,000 agents, wide Unicode,
//! the blocked panel, the reconnect banner and the Artifacts screen; agent rows are
//! grouped Working and Completed and only the visible ones are drawn; the dashboard keys cancel
//! (confirmed) and open the Artifacts screen, whose Enter downloads the selected artifact.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::pull::Pulled;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::{
    ArtifactView, DownloadUrlResponse, EventsPage, RunControlResponse,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];
const RUN: &str = "01K6A0000000000000000000R1";

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

thread_local! {
    static LAST_KEY: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// A key that arrives 400 ms after the one before it (the first one now): a person's pace, past the 300 ms a second `c` waits for the
/// preview of the cancel.
fn press_slowly(app: &mut App, c: KeyCode) -> Vec<Effect> {
    let at = LAST_KEY.with(|last| {
        let next = last.get().map_or_else(std::time::Instant::now, |t| {
            t + std::time::Duration::from_millis(400)
        });
        last.set(Some(next));
        next
    });
    app.handle_key_at(KeyEvent::new(c, KeyModifiers::NONE), at)
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
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
            // A wide character covers the cells after it.
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
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

fn snapshot_at(app: &App, name: &str, w: u16, h: u16) {
    insta::assert_snapshot!(format!("{name}_{w}x{h}"), screen_text(app, w, h));
}

fn snapshot_sizes(app: &App, name: &str) {
    for (w, h) in SIZES {
        snapshot_at(app, name, w, h);
    }
}

/// Event `n` (ids sort by `n`): an agent's own event when `agent` is set, else a harness event.
fn env(
    n: u32,
    t: EventType,
    task: Option<&str>,
    agent: Option<&str>,
    seq: u64,
    v: Value,
) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A000000000000000{n:06}"),
        run_id: RUN.into(),
        task_id: task.map(str::to_string),
        agent_id: agent.map(str::to_string),
        seq,
        ts: "2026-09-27T09:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn created() -> Envelope {
    env(
        0,
        EventType::RunCreated,
        None,
        None,
        0,
        json!({"target": {"mode": "s3-only"}}),
    )
}

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by_shard: BTreeMap<u8, Vec<Envelope>> = BTreeMap::new();
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

/// `n` agents, each with a session, a turn and a summary.
fn agents(n: u32) -> Vec<Envelope> {
    let mut ev = vec![created()];
    let mut k = 1;
    for i in 0..n {
        let a = format!("A{i:04}");
        let model = if i % 2 == 0 {
            "us.anthropic.claude-sonnet-5"
        } else {
            "qwen.qwen3-coder-480b-a35b-v1:0"
        };
        ev.push(env(
            k,
            EventType::SessionStarted,
            None,
            None,
            0,
            json!({"agentId": a, "engine": "cline", "modelId": model}),
        ));
        ev.push(env(
            k + 1,
            EventType::TurnStarted,
            None,
            Some(&a),
            0,
            json!({"turn": 1}),
        ));
        ev.push(env(
            k + 2,
            EventType::ToolSummary,
            None,
            Some(&a),
            0,
            json!({"toolCallId": format!("call-{i}"), "title": format!("editing src/module_{i}.py"), "kind": "edit", "status": "pending"}),
        ));
        k += 3;
    }
    ev
}

fn dashboard(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(events));
    app
}

#[test]
fn an_empty_run_and_one_agent() {
    snapshot_sizes(&dashboard(vec![created()]), "dash_empty_run");
    let app = dashboard(agents(1));
    snapshot_sizes(&app, "dash_1_agent");
    assert!(screen_text(&app, 80, 24).contains("A0000"));
}

#[test]
fn five_hundred_agents_are_grouped_and_scroll() {
    let mut app = dashboard(agents(500));
    snapshot_sizes(&app, "dash_500_agents");
    // The agents are a folded card at the end of the conversation, which follows its end: the card is a line, not 500 rows.
    let folded = screen_text(&app, 120, 40);
    assert!(folded.contains("agents 500"), "{folded}");
    assert!(!folded.contains("A0499"), "a folded card draws no rows");
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Char(' '));
    // The draw brings the selected card into view; End then goes to the end of the open card.
    let opened = screen_text(&app, 120, 40);
    assert!(opened.contains("Working (500)"), "{opened}");
    press(&mut app, KeyCode::End);
    let bottom = screen_text(&app, 120, 40);
    assert!(bottom.contains("A0499"), "the last agent: {bottom}");
    // Only the rows of the visible lines are drawn, and the group heading is in the card.
    assert!(!bottom.contains("A0001"), "{bottom}");
    press(&mut app, KeyCode::PageUp);
    let page = screen_text(&app, 120, 40);
    assert_ne!(page, bottom);
    press(&mut app, KeyCode::Home);
    let top = screen_text(&app, 120, 40);
    assert!(top.contains("ham \u{b7} Run"), "{top}");
    assert!(!top.contains("A0499"), "{top}");
    press(&mut app, KeyCode::End);
    assert_eq!(screen_text(&app, 120, 40), bottom);
    press(&mut app, KeyCode::Up);
    assert_ne!(screen_text(&app, 120, 40), bottom);
    press(&mut app, KeyCode::Down);
    assert_eq!(screen_text(&app, 120, 40), bottom);
}

/// The two times the kernel keeps for this thread: on a processor, and ready to run and waiting for one. They are the first two numbers
/// of `/proc/thread-self/schedstat`, in nanoseconds (Linux). `None` where the machine has no such file.
fn thread_times() -> Option<(Duration, Duration)> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let mut numbers = text
        .split_whitespace()
        .map(|n| n.parse().ok().map(Duration::from_nanos));
    Some((numbers.next()??, numbers.next()??))
}

/// The clock of the draw bound: the wall clock less the time the drawing thread waited for a processor, where the machine reports that
/// wait, and the wall clock elsewhere. A draw is the work of one thread. On a build host where every processor is taken the thread also
/// waits for its turn on one, which says nothing about the draw. Everything else between the two readings counts: the work of
/// the draw, and a wait of the draw's own (a sleep, a lock, a read that blocks).
struct DrawClock {
    wall: Instant,
    waited: Option<Duration>,
}

impl DrawClock {
    fn start() -> Self {
        // The wall clock first and the wait second here, the other way round in `elapsed`: a wait for a processor that falls between
        // the two readings is then counted, at either end.
        let wall = Instant::now();
        Self {
            wall,
            waited: thread_times().map(|t| t.1),
        }
    }

    fn elapsed(&self) -> Duration {
        let waited = thread_times().map(|t| t.1);
        let wall = self.wall.elapsed();
        match (self.waited, waited) {
            (Some(start), Some(now)) => wall.saturating_sub(now.saturating_sub(start)),
            _ => wall,
        }
    }

    /// What the clock reads, for a failure message.
    fn what(&self) -> &'static str {
        if self.waited.is_some() {
            "wall clock less the drawing thread's wait for a processor"
        } else {
            "wall clock"
        }
    }
}

/// The times of some draws, each read on the clock of the bound and on the wall clock.
struct Draws {
    n: u32,
    clock: Duration,
    wall: Duration,
    slowest: Duration,
    what: &'static str,
}

impl Draws {
    fn new() -> Self {
        Self {
            n: 0,
            clock: Duration::ZERO,
            wall: Duration::ZERO,
            slowest: Duration::ZERO,
            what: "no draw",
        }
    }

    /// One more draw, which began when `t` started and has just ended.
    fn add(&mut self, t: &DrawClock) {
        let read = t.elapsed();
        self.wall += t.wall.elapsed();
        self.n += 1;
        self.clock += read;
        self.slowest = self.slowest.max(read);
        self.what = t.what();
    }

    /// The seconds one draw took on the clock of the bound, the mean of the draws.
    fn per_draw(&self) -> f64 {
        self.clock.as_secs_f64() / f64::from(self.n)
    }

    fn on_the_wall(&self) -> f64 {
        self.wall.as_secs_f64() / f64::from(self.n)
    }
}

/// One more tool call of agent `A0000`, the `n`th.
fn row_change(n: u32) -> Envelope {
    env(
        20_000 + n,
        EventType::ToolSummary,
        None,
        Some("A0000"),
        u64::from(n) + 1,
        json!({"toolCallId": format!("call-again-{n}"), "title": format!("running the tests, pass {n}"), "kind": "execute", "status": "pending"}),
    )
}

/// Agent `A0000` reports one more tool call: its row of the agents card changes, as some row does all the time on a live run.
fn one_row_changes(app: &mut App, n: u32) {
    app.update(pages(vec![row_change(n)]));
}

#[test]
fn b836_a_draw_makes_only_the_rows_of_the_agents_that_changed() {
    let mut app = dashboard(agents(500));
    screen_text(&app, 120, 40);
    assert_eq!(app.agent_rows_made(), 500);
    screen_text(&app, 120, 40);
    assert_eq!(
        app.agent_rows_made(),
        500,
        "a second draw of the same screen made the rows of its agents again"
    );
    one_row_changes(&mut app, 0);
    screen_text(&app, 120, 40);
    assert_eq!(
        app.agent_rows_made(),
        501,
        "one agent's row changed, so the draw after it makes that row and no other"
    );
}

#[test]
fn b836_a_screen_drawn_before_shows_the_agents_a_first_draw_shows() {
    // One app draws, sees rows change between its draws and draws at every size. The other gets the same events at once: its first
    // draw makes every row in one go.
    let mut drawn = dashboard(agents(500));
    for (w, h) in SIZES {
        screen_text(&drawn, w, h);
    }
    for n in 0..3 {
        one_row_changes(&mut drawn, n);
        screen_text(&drawn, 120, 40);
    }
    let mut events = agents(500);
    events.extend((0..3).map(row_change));
    let mut first = dashboard(events);
    assert_eq!(screen_text(&drawn, 120, 40), screen_text(&first, 120, 40));
    assert_eq!(first.agent_rows_made(), 500);
    // The agents card, opened, from its end to its start.
    for app in [&mut drawn, &mut first] {
        press(app, KeyCode::BackTab);
        press(app, KeyCode::Char(' '));
        screen_text(app, 120, 40);
    }
    for key in [
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageUp,
        KeyCode::Home,
    ] {
        for app in [&mut drawn, &mut first] {
            press(app, key);
        }
        // Both are drawn at the same sizes in the same order: a page is as tall as the last draw.
        for (w, h) in SIZES {
            let (was_drawn, at_once) = (screen_text(&drawn, w, h), screen_text(&first, w, h));
            assert_eq!(was_drawn, at_once, "{key:?} at {w}x{h}");
            if key == KeyCode::End {
                // TRIGGER PRESENT: the screens that are compared show the rows of the opened agents card.
                assert!(
                    at_once.contains("A0499 "),
                    "the last agent's row at {w}x{h}: {at_once}"
                );
            }
        }
    }
}

#[test]
fn five_thousand_agents_draw_in_bounded_time() {
    const AGENTS: u64 = 5000;
    let mut app = dashboard(agents(5000));
    // The bound holds for each kind of draw on its own, so that no cheap kind hides a dear one in a mean:
    // - a whole draw makes every row of the agent table and makes the agents card into a block: the first draw of the screen and a
    //   draw after the layout of the table changed. Until every draw did both.
    // - a draw after one agent's row changed makes that row and the agents card again: the draw of a live run.
    // - a draw of the screen that did not change makes neither.
    let (mut whole, mut changed, mut again) = (Draws::new(), Draws::new(), Draws::new());
    // The snapshots first, each compared inside the timed part as it always was. What a draw made says which kind it is: the rows of
    // 120 and of 200 columns are the same text, so the third size makes its rows for another layout and no block.
    for (w, h) in SIZES {
        let (rows, blocks) = (app.agent_rows_made(), app.card_blocks_made());
        let t = DrawClock::start();
        snapshot_at(&app, "dash_5000_agents", w, h);
        let kind = match (
            app.agent_rows_made() - rows,
            app.card_blocks_made() - blocks,
        ) {
            (AGENTS, 1..) => &mut whole,
            (0, 0) => &mut again,
            _ => &mut changed,
        };
        kind.add(&t);
    }
    assert!(
        whole.n >= 1,
        "the first draw of the run screen did not make every row and the agents card"
    );
    // Whole draws until there are thirteen, as many draws as the bound was always the mean of. The table goes from its narrow layout
    // to its wide one and back: each draw makes every row, and the rows differ, so each makes the agents card.
    let mut size = (120, 40);
    while whole.n < 13 {
        size = if size == (80, 24) {
            (120, 40)
        } else {
            (80, 24)
        };
        let (rows, blocks) = (app.agent_rows_made(), app.card_blocks_made());
        let t = DrawClock::start();
        let screen = screen_text(&app, size.0, size.1);
        whole.add(&t);
        // TRIGGER PRESENT: the draw made every row and the agents card.
        assert_eq!(
            app.agent_rows_made() - rows,
            AGENTS,
            "the draw at {size:?} after another layout did not make every row"
        );
        assert!(
            app.card_blocks_made() > blocks,
            "the draw at {size:?} after another layout made no card into a block"
        );
        assert!(screen.contains("agents 5000"), "{screen}");
    }
    // Ten draws of a live run: one agent's row changed before each.
    for n in 0..10 {
        one_row_changes(&mut app, n);
        let (rows, blocks) = (app.agent_rows_made(), app.card_blocks_made());
        let t = DrawClock::start();
        let screen = screen_text(&app, size.0, size.1);
        changed.add(&t);
        // TRIGGER PRESENT: the draw made the row that changed, and the agents card again.
        assert_eq!(
            app.agent_rows_made() - rows,
            1,
            "draw {n} after one agent's row changed"
        );
        assert!(
            app.card_blocks_made() > blocks,
            "draw {n} after one agent's row changed made no card into a block"
        );
        assert!(screen.contains("agents 5000"), "{screen}");
    }
    // The same screen ten more times: nothing changed, so no row and no card is made again.
    for _ in 0..10 {
        let (rows, blocks) = (app.agent_rows_made(), app.card_blocks_made());
        let t = DrawClock::start();
        screen_text(&app, size.0, size.1);
        again.add(&t);
        assert_eq!(
            (
                app.agent_rows_made() - rows,
                app.card_blocks_made() - blocks
            ),
            (0, 0),
            "a draw of the screen that did not change made rows or cards again"
        );
    }
    // The figures of a run, shown with `--nocapture` and in the output of a failure.
    eprintln!(
        "DRAW-TIMES 5,000 agents: a whole draw {:.3} s (the slowest of {} {:.3} s; {:.3} s on the wall clock), a draw after one row changed {:.3} s (the slowest of {} {:.3} s; {:.3} s on the wall clock), a draw of the unchanged screen {:.3} s ({:.3} s on the wall clock); {}",
        whole.per_draw(),
        whole.n,
        whole.slowest.as_secs_f64(),
        whole.on_the_wall(),
        changed.per_draw(),
        changed.n,
        changed.slowest.as_secs_f64(),
        changed.on_the_wall(),
        again.per_draw(),
        again.on_the_wall(),
        whole.what
    );
    for (draws, kind) in [
        (&whole, "that makes every row and the agents card"),
        (&changed, "after one agent's row changed"),
        (&again, "of a screen that did not change"),
    ] {
        assert!(
            draws.per_draw() < 0.5,
            "a draw of 5,000 agents {kind} took {:.3} s ({}; {:.3} s on the wall clock; the slowest of {} took {:.3} s)",
            draws.per_draw(),
            draws.what,
            draws.on_the_wall(),
            draws.n,
            draws.slowest.as_secs_f64()
        );
    }
    assert!(screen_text(&app, 200, 60).contains("agents 5000"));
}

#[test]
fn b836_the_draw_clock_counts_a_wait_inside_the_draw() {
    // a draw that sleeps, waits for a lock or reads a slow disk is a slow draw: the clock of the bound has to show it
    let t = DrawClock::start();
    std::thread::sleep(Duration::from_millis(300));
    let read = t.elapsed();
    assert!(
        read >= Duration::from_millis(290),
        "a wait of 300 ms inside a draw counted as {read:?} ({})",
        t.what()
    );
}

#[test]
fn b836_the_draw_clock_counts_the_work_of_its_thread() {
    // the clock goes on while the thread computes, and never faster than the wall clock
    let wall = Instant::now();
    let t = DrawClock::start();
    let mut n = 0u64;
    while t.elapsed() < Duration::from_millis(100) {
        assert!(
            wall.elapsed() < Duration::from_secs(60),
            "the clock stood still for a minute of work"
        );
        n = std::hint::black_box(n.wrapping_mul(31).wrapping_add(7));
    }
    let (worked, passed) = (t.elapsed(), wall.elapsed());
    assert!(worked <= passed, "{worked:?} of work in {passed:?}");
}

/// Puts this thread, and the threads it starts from now on, on the one processor it runs on (`taskset` of util-linux). `false` where
/// that cannot be done.
fn stay_on_one_processor() -> bool {
    let thread = std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|p| p.file_name()?.to_str()?.parse::<u32>().ok());
    // the 39th field of `stat` is the processor the thread ran on last; the fields after the name in brackets start with the 3rd
    let processor = std::fs::read_to_string("/proc/thread-self/stat")
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(36)?
                .parse::<u32>()
                .ok()
        });
    let (Some(thread), Some(processor)) = (thread, processor) else {
        return false;
    };
    let processor = processor.to_string();
    let set = std::process::Command::new("taskset")
        .args(["-p", "-c", &processor, &thread.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    set && std::fs::read_to_string("/proc/thread-self/status").is_ok_and(|s| {
        s.lines().any(|l| {
            l.strip_prefix("Cpus_allowed_list:").map(str::trim) == Some(processor.as_str())
        })
    })
}

#[test]
fn b836_the_draw_clock_leaves_out_the_wait_for_a_processor() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    if thread_times().is_none() {
        eprintln!(
            "NOT RUN: this machine reports no scheduling times of a thread; the draw bound reads the wall clock here"
        );
        return;
    }
    if !stay_on_one_processor() {
        eprintln!(
            "NOT RUN: this thread could not be put on one processor with `taskset`, so no wait for a processor can be made here"
        );
        return;
    }
    // a second thread on the same processor that does nothing but use it: this thread now waits for its turn about half of the time
    let (stop, running) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let busy = {
        let (stop, running) = (stop.clone(), running.clone());
        std::thread::spawn(move || {
            running.store(true, Ordering::SeqCst);
            while !stop.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        })
    };
    while !running.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    let wall = Instant::now();
    let t = DrawClock::start();
    let before = thread_times().unwrap();
    let mut n = 0u64;
    while wall.elapsed() < Duration::from_millis(400) {
        n = std::hint::black_box(n.wrapping_mul(31).wrapping_add(7));
    }
    let read = t.elapsed();
    let passed = wall.elapsed();
    let after = thread_times().unwrap();
    stop.store(true, Ordering::Relaxed);
    busy.join().unwrap();
    if after.0.is_zero() {
        eprintln!(
            "NOT RUN: the kernel keeps no scheduling times of a thread here; the draw bound reads the wall clock"
        );
        return;
    }
    let (worked, waited) = (after.0 - before.0, after.1 - before.1);
    // TRIGGER PRESENT: the thread did wait for its processor
    assert!(
        waited >= Duration::from_millis(100),
        "the thread shared one processor with a busy thread for {passed:?} and waited only {waited:?} for it"
    );
    // The wait is left out: what the clock read is the work of the thread, as the kernel counted it (the kernel adds a running
    // thread's work at its timer tick, up to 10 ms late).
    assert!(
        read.abs_diff(worked) <= Duration::from_millis(30),
        "the clock read {read:?} for {worked:?} of work in {passed:?}, {waited:?} of them spent waiting for the processor"
    );
}

#[test]
fn wide_unicode_text_keeps_the_columns() {
    let mut ev = vec![created()];
    for (i, (who, text)) in [
        ("A-数据", "修复 数据库 连接 🚀 并添加测试"),
        ("A-emoji", "👩‍💻 writes tests ✓ for ütf-8 naïve café"),
        ("A-rtl", "مرحبا \u{202e}word\u{202c} text"),
    ]
    .iter()
    .enumerate()
    {
        let n = i as u32 * 3 + 1;
        ev.push(env(
            n,
            EventType::SessionStarted,
            None,
            None,
            0,
            json!({"agentId": who, "engine": "cline", "modelId": "us.anthropic.claude-sonnet-5"}),
        ));
        ev.push(env(
            n + 1,
            EventType::ToolSummary,
            None,
            Some(who),
            0,
            json!({"title": text, "kind": "edit", "status": "pending"}),
        ));
    }
    let app = dashboard(ev);
    snapshot_sizes(&app, "dash_wide_unicode");
    let t = screen_text(&app, 200, 60);
    assert!(t.contains("数据") && t.contains("🚀"), "{t}");
    assert!(!t.contains('\u{202e}'), "bidi overrides are removed");
}

fn blocked_events() -> Vec<Envelope> {
    vec![
        created(),
        env(
            1,
            EventType::SessionStarted,
            None,
            None,
            0,
            json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-sonnet-5"}),
        ),
        env(
            2,
            EventType::TurnEnded,
            None,
            Some("direct"),
            0,
            json!({"turn": 1, "outcome": "completed", "stopReason": "end_turn"}),
        ),
        env(
            3,
            EventType::GoalBlocked,
            None,
            None,
            0,
            json!({"reason": "the session ended before any work was saved"}),
        ),
    ]
}

#[test]
fn the_blocked_panel_shows_the_reason() {
    let mut app = dashboard(blocked_events());
    snapshot_sizes(&app, "dash_blocked_panel");
    for (w, h) in SIZES {
        let t = screen_text(&app, w, h);
        for want in [
            "blocked · r retry",
            "reason: the session ended before any work was saved",
        ] {
            assert!(t.contains(want), "{w}x{h} lacks {want:?}:\n{t}");
        }
    }
    // The blocked panel offers the retry and no resume (there is no pause or resume on the direct line), and no cancel: the run has ended.
    assert!(press(&mut app, KeyCode::Char('R')).is_empty());
    assert!(press(&mut app, KeyCode::Char('p')).is_empty());
}

#[test]
fn a_failed_poll_shows_the_reconnect_banner_until_a_poll_succeeds() {
    let mut app = dashboard(agents(3));
    let refused = || {
        Msg::Pages(Err(
            "network: GET http://127.0.0.1/live/v1/runs/R1/events: connection refused".into(),
        ))
    };
    app.update(refused());
    // Inside the grace of one second nothing is shown.
    assert!(!screen_text(&app, 80, 24).contains("Connection lost"));
    app.tick();
    app.update(refused());
    app.tick();
    app.update(refused());
    snapshot_sizes(&app, "dash_reconnect");
    let t = screen_text(&app, 80, 24);
    assert!(t.contains("Connection lost"), "{t}");
    assert!(screen_text(&app, 200, 60).contains("the run keeps going in AWS"));
    app.update(pages(vec![]));
    assert!(!screen_text(&app, 80, 24).contains("Connection lost"));
}

#[test]
fn a_confirmed_cancel() {
    let mut app = dashboard(agents(2));
    let cancel = || vec![Effect::Cancel { run_id: RUN.into() }];
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
    assert!(screen_text(&app, 120, 40).contains("press c again to cancel"));
    assert_eq!(press_slowly(&mut app, KeyCode::Char('c')), cancel());
    app.update(Msg::Cancelled(Ok(RunControlResponse {
        run_id: RUN.into(),
        status: GoalStatus::Cleared,
    })));
    assert!(screen_text(&app, 120, 40).contains(&format!("run {RUN} cleared")));
    // Another key clears the confirmation.
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
    press_slowly(&mut app, KeyCode::Down);
    assert!(press_slowly(&mut app, KeyCode::Char('c')).is_empty());
    app.update(Msg::Cancelled(Err(
        "HTTP 409: run R1 is complete (run-finished)".into(),
    )));
    assert!(screen_text(&app, 120, 40).contains("run-finished"));
}

fn artifact(id: &str, kind: ArtifactKind, key: &str, size: u64) -> ArtifactView {
    ArtifactView {
        artifact_id: id.into(),
        kind,
        s3_uri: format!("s3://hamh-live-harness-800000000022/{key}"),
        sha256: format!("{:0>64}", id.to_lowercase()),
        size_bytes: size,
    }
}

#[test]
fn the_artifacts_screen_lists_and_downloads_the_runs_artifacts() {
    let mut app = dashboard(agents(1));
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        vec![Effect::LoadArtifacts { run_id: RUN.into() }]
    );
    assert_eq!(app.screen(), Screen::Artifacts);
    assert!(screen_text(&app, 80, 24).contains("loading the artifact list"));
    let list = vec![
        artifact(
            "01K6A00000000000000000ZP01",
            ArtifactKind::Zip,
            &format!("runs/{RUN}/deliverable/python-calc-{RUN}-abc1234.zip"),
            48_213,
        ),
        artifact(
            "01K6A00000000000000000EV01",
            ArtifactKind::Evidence,
            &format!("runs/{RUN}/evidence/evidence.zip"),
            9_120,
        ),
        artifact(
            "01K6A00000000000000000PT01",
            ArtifactKind::Partial,
            &format!("runs/{RUN}/deliverable/partial/python-calc-{RUN}-abc1234.zip"),
            20_480_000,
        ),
    ];
    app.update(Msg::Artifacts(Ok(list.clone())));
    snapshot_sizes(&app, "artifacts_screen");
    press(&mut app, KeyCode::Down);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        vec![Effect::DownloadArtifact {
            run_id: RUN.into(),
            artifact_id: "01K6A00000000000000000EV01".into()
        }]
    );
    assert!(screen_text(&app, 120, 40).contains("downloading evidence.zip"));
    app.update(Msg::ArtifactPulled(Ok(Pulled {
        path: PathBuf::from("/downloads/evidence.zip"),
        artifact: list[1].clone(),
        url: DownloadUrlResponse {
            url: "https://b.s3.amazonaws.com/x?X-Amz-Signature=s".into(),
            expires_at: "2026-09-27T09:15:00Z".into(),
        },
        ranges: vec![(0, 9119)],
        resumed_from: 0,
    })));
    let t = screen_text(&app, 120, 40);
    // The harness does not claim to have verified anything; the notice says the checksum matched (defect F1).
    assert!(
        t.contains("saved /downloads/evidence.zip (sha256 matched)"),
        "{t}"
    );
    assert_eq!(harness_protocol::direct::banned_claim_word(&t), None, "{t}");
    assert!(
        !t.contains("X-Amz-Signature"),
        "no presigned query on screen"
    );
    app.update(Msg::ArtifactPulled(Err("download: sha256 mismatch".into())));
    assert!(screen_text(&app, 120, 40).contains("download failed: download: sha256 mismatch"));
    app.update(Msg::Artifacts(Err("HTTP 503: unavailable".into())));
    assert!(screen_text(&app, 120, 40).contains("cannot list artifacts: HTTP 503"));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        vec![Effect::LoadArtifacts { run_id: RUN.into() }]
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Dashboard);
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
}
