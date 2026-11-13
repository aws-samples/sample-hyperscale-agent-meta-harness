//! The status line, the reconnect overlay and the alerts of the run screen. The pure parts are `ham_core::meters`
//! and `ham_core::alert`; these tests drive the screen. The header counts the elapsed time while the run goes on (and stops when it
//! ends), shows the tokens and cost "as of turn k" and asks for the totals again when a turn ends, and says how long ago the run was last
//! read; a failed poll shows the overlay only after the grace (1 s, or 5 s before the first good read) and clears it on the next good
//! read; a question that starts to wait rings the bell once and, when the option is on and the window is not focused, sends a terminal
//! notification; the Runs list does the same for a run that starts to wait, and its first read makes no sound.
#![allow(clippy::unwrap_used)]

use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::alert::{Alert, notification_bytes};
use ham_core::runfilter::{IndexedRead, RunsRead};
use harness_protocol::api::v1::{EventsPage, RunDetail, RunSummary};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const START: &str = "2026-10-14T20:00:00Z";

fn screen(app: &App) -> String {
    screen_at(app, 140, 40)
}

fn screen_at(app: &App, w: u16, h: u16) -> String {
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

fn first_row(app: &App) -> String {
    screen(app).lines().next().unwrap().to_string()
}

fn at(ts: &str, n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: ts.into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    at(START, n, t, agent, v)
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

fn start_events() -> Vec<Envelope> {
    vec![
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
    ]
}

fn turn_ended(n: u32) -> Envelope {
    env(
        n,
        EventType::TurnEnded,
        Some("direct"),
        json!({"summary": "done with this prompt"}),
    )
}

fn detail(tokens: u64, cost: f64) -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": START, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": tokens, "costUsd": cost
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

fn question(n: u32, request: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({
            "turn": 1, "kind": "needs-input",
            "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
            "requestId": request, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
            "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": "x", "runS": 412, "sessions": 1},
            "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
            "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-14T20:30:00Z"
        }),
    )
}

fn created() -> i64 {
    parse_utc_unix(START).unwrap()
}

/// The run on screen with its events and detail read.
fn on_screen(extra: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(created() + 760);
    app.open_run(RUN);
    let mut events = start_events();
    events.extend(extra);
    app.update(pages(events));
    app.update(detail(113_000, 0.14));
    app
}

// ---- the header -------------------------------------------------------------------------------------------------------------------

#[test]
fn the_header_counts_the_elapsed_time_while_the_run_goes_on() {
    let mut app = on_screen(vec![]);
    let row = first_row(&app);
    assert!(row.contains("12m40s"), "{row}");
    app.set_now(created() + 765);
    assert!(first_row(&app).contains("12m45s"), "{}", first_row(&app));
    app.set_now(created() + 3_725);
    assert!(first_row(&app).contains("1h02m"), "{}", first_row(&app));
}

#[test]
fn a_finished_run_stops_counting_at_the_time_it_ended() {
    let mut app = on_screen(vec![
        env(
            4,
            EventType::GoalStatus,
            None,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        at(
            "2026-10-14T20:30:00Z",
            5,
            EventType::RunCompleted,
            None,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]);
    app.set_now(created() + 7_200);
    let row = first_row(&app);
    assert!(row.contains("30m00s"), "{row}");
    app.set_now(created() + 9_000);
    assert!(first_row(&app).contains("30m00s"));
}

#[test]
fn the_tokens_and_the_cost_say_as_of_which_turn() {
    let app = on_screen(vec![turn_ended(10), turn_ended(11), turn_ended(12)]);
    let row = first_row(&app);
    assert!(row.contains("113k tok"), "{row}");
    assert!(row.contains("$0.14"), "{row}");
    assert!(row.contains("as of turn 3"), "{row}");
}

/// A detail whose `tokensUsed` is 0 (a stage that does not count them, such as the direct line before `get_run` summed its turns) and whose
/// `cost` carries the usage of its turns.
fn detail_with_lines(tokens_used: u64, usage: [u64; 4]) -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": START, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": tokens_used, "costUsd": 0.14,
        "cost": {"pricesVersion": "p", "turnsWithoutUsage": 0, "lines": [
            {"modelId": MODEL, "engine": "cline", "usd": 0.14, "usage": {
                "inputTokens": usage[0], "outputTokens": usage[1],
                "cacheReadInputTokens": usage[2], "cacheWriteInputTokens": usage[3]}}]}
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

#[test]
fn a_stage_that_reports_no_tokens_still_shows_the_tokens_its_cost_lines_carry() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(created() + 760);
    app.open_run(RUN);
    app.update(pages(start_events()));
    app.update(detail_with_lines(0, [100_000, 13_000, 0, 0]));
    let row = first_row(&app);
    assert!(
        row.contains("113k tok"),
        "the four kinds of token add up as the final card counts them: {row}"
    );
    // The stage's own number wins when it has one; a detail with neither shows no counter.
    app.update(detail_with_lines(200_000, [1, 2, 3, 4]));
    assert!(first_row(&app).contains("200k tok"), "{}", first_row(&app));
    app.update(detail(0, 0.0));
    assert!(!first_row(&app).contains(" tok"), "{}", first_row(&app));
}

#[test]
fn a_run_with_no_turn_yet_adds_no_turn_label() {
    let app = on_screen(vec![]);
    assert!(
        !first_row(&app).contains("as of turn"),
        "{}",
        first_row(&app)
    );
}

#[test]
fn a_turn_that_ends_asks_for_the_totals_again_and_a_quiet_poll_does_not() {
    let mut app = on_screen(vec![]);
    let asks = |e: &[Effect]| {
        e.iter()
            .filter(|e| matches!(e, Effect::LoadDetail { run_id } if run_id == RUN))
            .count()
    };
    let effects = app.update(pages(vec![turn_ended(10)]));
    assert_eq!(asks(&effects), 1, "{effects:?}");
    // The same poll read again, and one with no turn in it, ask for nothing.
    let effects = app.update(pages(vec![turn_ended(10)]));
    assert_eq!(asks(&effects), 0, "{effects:?}");
    let effects = app.update(pages(vec![env(
        20,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "message", "text": "still working"}),
    )]));
    assert_eq!(asks(&effects), 0, "{effects:?}");
    let effects = app.update(pages(vec![turn_ended(21)]));
    assert_eq!(asks(&effects), 1, "{effects:?}");
}

#[test]
fn the_header_says_how_long_ago_the_run_was_last_read() {
    let mut app = on_screen(vec![]);
    assert!(
        first_row(&app).contains("updated 0s ago"),
        "{}",
        first_row(&app)
    );
    app.tick();
    app.tick();
    assert!(
        first_row(&app).contains("updated 2s ago"),
        "{}",
        first_row(&app)
    );
    // A good read starts the count again.
    app.update(pages(vec![]));
    assert!(
        first_row(&app).contains("updated 0s ago"),
        "{}",
        first_row(&app)
    );
}

// ---- the reconnect overlay --------------------------------------------------------------------------------------------------------

#[test]
fn a_failed_poll_shows_the_overlay_after_one_second_and_the_next_good_read_clears_it() {
    let mut app = on_screen(vec![]);
    app.tick(); // 1 s after the good read
    app.update(Msg::Pages(Err("timeout".into())));
    // Inside the grace nothing is shown.
    assert!(
        !screen(&app).contains("Connection lost"),
        "{}",
        screen(&app)
    );
    app.tick();
    app.update(Msg::Pages(Err("timeout".into())));
    let text = screen(&app);
    assert!(text.contains("Connection lost, retrying (1 s)"), "{text}");
    assert!(text.contains("timeout"), "{text}");
    assert!(text.contains("the run keeps going in AWS"), "{text}");
    // The longer it lasts, the longer it says.
    app.tick();
    app.update(Msg::Pages(Err("timeout".into())));
    assert!(
        screen(&app).contains("Connection lost, retrying (2 s)"),
        "{}",
        screen(&app)
    );
    // A good read clears it at once.
    app.update(pages(vec![]));
    assert!(
        !screen(&app).contains("Connection lost"),
        "{}",
        screen(&app)
    );
}

#[test]
fn before_the_first_good_read_the_grace_is_five_seconds_and_the_words_differ() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(Msg::Pages(Err("no route".into())));
    for _ in 0..4 {
        app.tick();
        app.update(Msg::Pages(Err("no route".into())));
        assert!(!screen(&app).contains("retrying"), "{}", screen(&app));
    }
    app.tick();
    app.update(Msg::Pages(Err("no route".into())));
    let text = screen(&app);
    assert!(
        text.contains("Cannot connect yet, retrying (5 s)"),
        "{text}"
    );
}

// ---- the alerts -------------------------------------------------------------------------------------------------------------------

fn rings(effects: &[Effect]) -> usize {
    effects.iter().filter(|e| **e == Effect::Bell).count()
}

fn notifications(effects: &[Effect]) -> Vec<&Alert> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(a) => Some(a),
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_question_rings_the_bell_once_and_sends_no_notification_by_default() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut events = start_events();
    events.push(question(40, REQ));
    let effects = app.update(pages(events));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    assert!(notifications(&effects).is_empty(), "{effects:?}");
}

#[test]
fn with_the_option_on_a_notification_goes_only_while_the_window_is_not_focused() {
    for (focused, expect) in [(true, 0usize), (false, 1)] {
        let mut app = App::new_direct("n".into(), None, DirectSettings::default());
        app.set_notify(true);
        app.set_focus(focused);
        app.open_run(RUN);
        let mut events = start_events();
        events.push(question(40, REQ));
        let effects = app.update(pages(events));
        // The bell does not depend on focus.
        assert_eq!(rings(&effects), 1, "focused={focused}: {effects:?}");
        let n = notifications(&effects);
        assert_eq!(n.len(), expect, "focused={focused}: {effects:?}");
        if let Some(a) = n.first() {
            let mut bytes = Vec::new();
            a.write_to(&mut bytes).unwrap();
            assert_eq!(
                bytes,
                notification_bytes(RUN),
                "the bytes are built from fixed words and the run id"
            );
            assert!(!a.bell, "one bell for the question, not two");
        }
    }
}

fn waiting_run(id: &str, waiting: bool) -> RunSummary {
    let mut v = json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": START, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"});
    if waiting {
        v["needsInput"] = json!({"requestId": REQ, "reason": "engine_stop", "title": "Cline stopped",
                                 "expiresAt": "2026-10-14T20:30:00Z"});
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn the_runs_list_rings_for_a_run_that_starts_to_wait_and_not_for_the_first_read() {
    const A: &str = "01K6A0000000000000000000A1";
    const B: &str = "01K6A0000000000000000000B2";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    // The first read makes no sound, even with a run that already waits.
    let effects = app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, false),
    ])));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    // The same list again, no sound.
    let effects = app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, false),
    ])));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    // B starts to wait: one bell.
    let effects = app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, true),
    ])));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    // Both still wait: no sound; B stops and starts again: it rings again.
    let effects = app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, true),
    ])));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, false),
    ])));
    let effects = app.update(Msg::Runs(Ok(vec![
        waiting_run(A, true),
        waiting_run(B, true),
    ])));
    assert_eq!(rings(&effects), 1, "{effects:?}");
}

#[test]
fn the_runs_list_sends_the_notification_for_the_run_that_started_to_wait() {
    const A: &str = "01K6A0000000000000000000A1";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.set_focus(false);
    app.update(Msg::Runs(Ok(vec![waiting_run(A, false)])));
    let effects = app.update(Msg::Runs(Ok(vec![waiting_run(A, true)])));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    let n = notifications(&effects);
    assert_eq!(n.len(), 1, "{effects:?}");
    assert_eq!(n[0].run_id, A);
}

// ---- the Runs list as the compiled app reads it: `Effect::LoadRuns` and the timer answer with `Msg::RunsIndexed` -----------------------

/// A read of every page, as `Effect::LoadRuns` and a full refresh answer it.
fn indexed(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index: vec![],
    }))
}

/// A read of the newest page laid over the list held, as the 5 s timer answers it.
fn indexed_newest(runs: Vec<RunSummary>) -> Msg {
    Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::First {
            runs,
            has_more: false,
        },
        index: vec![],
    }))
}

#[test]
fn the_indexed_read_of_the_runs_list_rings_for_a_run_that_starts_to_wait_and_not_for_the_first_read()
 {
    const A: &str = "01K6A0000000000000000000A1";
    const B: &str = "01K6A0000000000000000000B2";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    // The first read makes no sound, even with a run that already waits.
    let effects = app.update(indexed(vec![waiting_run(A, true), waiting_run(B, false)]));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    // The same list again (the timer), no sound.
    let effects = app.update(indexed_newest(vec![
        waiting_run(A, true),
        waiting_run(B, false),
    ]));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    // B starts to wait: one bell, not one for every poll that follows.
    let effects = app.update(indexed_newest(vec![
        waiting_run(A, true),
        waiting_run(B, true),
    ]));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    for _ in 0..3 {
        let effects = app.update(indexed_newest(vec![
            waiting_run(A, true),
            waiting_run(B, true),
        ]));
        assert_eq!(
            rings(&effects),
            0,
            "a run that keeps waiting is silent: {effects:?}"
        );
    }
    // B is answered and waits again: it rings again.
    app.update(indexed_newest(vec![
        waiting_run(A, true),
        waiting_run(B, false),
    ]));
    let effects = app.update(indexed(vec![waiting_run(A, true), waiting_run(B, true)]));
    assert_eq!(rings(&effects), 1, "{effects:?}");
}

#[test]
fn the_indexed_read_rings_once_for_a_batch_of_runs_that_start_to_wait_together() {
    const A: &str = "01K6A0000000000000000000A1";
    const B: &str = "01K6A0000000000000000000B2";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(indexed(vec![waiting_run(A, false), waiting_run(B, false)]));
    let effects = app.update(indexed_newest(vec![
        waiting_run(A, true),
        waiting_run(B, true),
    ]));
    assert_eq!(rings(&effects), 1, "one bell for the batch: {effects:?}");
}

#[test]
fn the_indexed_read_sends_the_notification_for_the_run_that_started_to_wait() {
    const A: &str = "01K6A0000000000000000000A1";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.set_focus(false);
    app.update(indexed(vec![waiting_run(A, false)]));
    let effects = app.update(indexed_newest(vec![waiting_run(A, true)]));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    let n = notifications(&effects);
    assert_eq!(n.len(), 1, "{effects:?}");
    assert_eq!(n[0].run_id, A);
    assert!(!n[0].bell, "one bell for the run, not two");
    // The same notification, once: the run keeps waiting.
    let effects = app.update(indexed_newest(vec![waiting_run(A, true)]));
    assert!(
        effects.is_empty(),
        "no repeated bell or notification: {effects:?}"
    );
}

#[test]
fn the_indexed_read_sends_no_notification_while_the_window_is_focused() {
    const A: &str = "01K6A0000000000000000000A1";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.update(indexed(vec![waiting_run(A, false)]));
    let effects = app.update(indexed_newest(vec![waiting_run(A, true)]));
    assert_eq!(rings(&effects), 1, "the bell does not depend on focus");
    assert!(notifications(&effects).is_empty(), "{effects:?}");
}

#[test]
fn with_the_bell_off_the_indexed_read_rings_nothing_and_still_notifies_when_asked() {
    const A: &str = "01K6A0000000000000000000A1";
    // Bell off, notification off: silence.
    let mut quiet = App::new_direct("n".into(), None, DirectSettings::default());
    quiet.set_bell(false);
    quiet.update(indexed(vec![waiting_run(A, false)]));
    let effects = quiet.update(indexed_newest(vec![waiting_run(A, true)]));
    assert!(effects.is_empty(), "{effects:?}");
    // Bell off, notification on and the window away: the notification only.
    let mut away = App::new_direct("n".into(), None, DirectSettings::default());
    away.set_bell(false);
    away.set_notify(true);
    away.set_focus(false);
    away.update(indexed(vec![waiting_run(A, false)]));
    let effects = away.update(indexed_newest(vec![waiting_run(A, true)]));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    assert_eq!(notifications(&effects).len(), 1, "{effects:?}");
}

#[test]
fn the_three_reads_of_the_list_share_one_memory_of_which_runs_wait() {
    const A: &str = "01K6A0000000000000000000A1";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    // One kind of read starts the memory, another reads the run waiting for the first time: one bell. Then every kind is silent.
    app.update(Msg::Runs(Ok(vec![waiting_run(A, false)])));
    let effects = app.update(indexed_newest(vec![waiting_run(A, true)]));
    assert_eq!(rings(&effects), 1, "{effects:?}");
    for msg in [
        Msg::Runs(Ok(vec![waiting_run(A, true)])),
        Msg::RunsRead(Ok(RunsRead::All(vec![waiting_run(A, true)]))),
        indexed(vec![waiting_run(A, true)]),
    ] {
        let effects = app.update(msg);
        assert!(effects.is_empty(), "{effects:?}");
    }
    // And the other way round: a run first seen waiting by the indexed read is not announced again by `Msg::RunsRead`.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(indexed(vec![waiting_run(A, false)]));
    app.update(indexed_newest(vec![waiting_run(A, true)]));
    let effects = app.update(Msg::RunsRead(Ok(RunsRead::All(vec![waiting_run(A, true)]))));
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn a_failed_indexed_read_makes_no_sound_and_a_run_that_starts_to_wait_after_it_rings_once() {
    const A: &str = "01K6A0000000000000000000A1";
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(indexed(vec![waiting_run(A, false)]));
    let effects = app.update(Msg::RunsIndexed(Err("timeout".into())));
    assert!(effects.is_empty(), "{effects:?}");
    let effects = app.update(indexed_newest(vec![waiting_run(A, true)]));
    assert_eq!(rings(&effects), 1, "{effects:?}");
}

// ---- a provider switch -----------------------------------------------------------------------------------------------

const SOL: &str = "us.openai.gpt-6.1-sol";

/// The `provider-switch` progress event agentd's collector writes (`ham-core/tests/notices.rs` holds its members to the collector's source).
fn switched(n: u32, kind: &str, from: &str, to: &str) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-switch", "n": 1, "from": from, "to": to, "switchKind": kind,
               "class": "transient", "detail": "ServiceUnavailableException"}),
    )
}

/// The reducer's view of the same events: the cause of the tests below. A test that finds no switch here would pass without testing the screen.
fn route_of(extra: &[Envelope]) -> ham_core::state::RunView {
    let mut s = ham_core::state::RunState::new(RUN);
    for e in start_events().into_iter().chain(extra.iter().cloned()) {
        s.apply(e);
    }
    s.view()
}

#[test]
fn before_a_switch_the_header_names_the_model_the_run_started_on() {
    let app = on_screen(vec![]);
    let row = first_row(&app);
    assert!(row.contains("Claude Sonnet 5.5"), "{row}");
    assert!(!row.contains("GPT-6.1 Sol"), "{row}");
}

#[test]
fn a_provider_switch_shows_in_the_conversation_with_the_model_it_went_to() {
    let events = vec![switched(10, "model", MODEL, SOL)];
    // TRIGGER PRESENT: the run switched to Sol.
    assert_eq!(route_of(&events).route.model.as_deref(), Some(SOL));
    let app = on_screen(events);
    let text = screen(&app);
    assert!(
        text.contains(
            "Switched to another model (switch 1): Claude Sonnet 5.5 to GPT-6.1 Sol, after a temporary failure."
        ),
        "{text}"
    );
}

/// The status line after a switch. The header
/// names the model of the route in use, so a run that failed over from Claude to GPT-6.1 Sol reads GPT-6.1 Sol. found the header
/// still naming the model `session.started` named and left this test ignored built the header and removed the `ignore`.
#[test]
fn after_a_provider_switch_the_status_line_names_the_model_the_run_is_on() {
    let events = vec![switched(10, "model", MODEL, SOL)];
    assert_eq!(route_of(&events).route.model.as_deref(), Some(SOL));
    let app = on_screen(events);
    let row = first_row(&app);
    assert!(row.contains("GPT-6.1 Sol"), "the header reads: {row}");
    assert!(
        !row.contains("Claude Sonnet 5.5"),
        "the header reads: {row}"
    );
}

/// The route line (`running on GPT-6.1 Sol after 1 switch`, `provider error, try 2 of 4`, `waiting on the model provider`) is the status line's
/// route part according to `ham-core`; the status block of the chat view draws it as its Route row while the run goes on.
#[test]
fn the_chat_view_draws_the_route_line_of_a_switch_a_retry_and_a_wait() {
    let retry = env(
        11,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-retry", "attempt": 2, "of": 4, "class": "transient", "backoffS": 12,
               "detail": "ThrottlingException"}),
    );
    let wait = env(
        12,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-wait", "pass": 1, "waitS": 60, "detail": "every route is down"}),
    );
    for events in [
        vec![switched(10, "model", MODEL, SOL)],
        vec![retry],
        vec![wait],
    ] {
        let want = route_of(&events)
            .route_line()
            .expect("TRIGGER PRESENT: the events give a route line");
        let text = screen(&on_screen(events));
        assert!(text.contains(&want), "the screen lacks {want:?}:\n{text}");
    }
}

// ---- the route in the header and in the status block ------------------------------------------------------------

const SONNET_US: &str = "us.anthropic.claude-sonnet-5-5";

fn retried(n: u32, attempt: u32) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-retry", "attempt": attempt, "of": 4, "class": "transient", "backoffS": 12,
               "detail": "ThrottlingException"}),
    )
}

fn waited(n: u32) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-wait", "pass": 1, "waitS": 60, "detail": "every route is down"}),
    )
}

fn outage(n: u32) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "provider-outage", "waitedS": 1800, "outageMinutes": 30}),
    )
}

/// The engine works again: a new turn starts.
fn turn_started(n: u32) -> Envelope {
    env(
        n,
        EventType::TurnStarted,
        Some("direct"),
        json!({"turn": 2}),
    )
}

/// Whether the screen has a Route row in its status block: the word alone, because `screen` drops the blanks at the end of a line (a row with
/// nothing after the word would otherwise slip by).
fn has_route_row(text: &str) -> bool {
    text.lines().any(|l| l.contains("Route"))
}

/// The line of the status block that starts with `label`, or the whole screen when there is none.
fn block_row(app: &App, label: &str) -> String {
    let text = screen(app);
    text.lines()
        .find(|l| l.contains(label))
        .map_or(text.clone(), str::to_string)
}

#[test]
fn a_retry_shows_in_the_header_next_to_the_state_until_the_engine_works_again() {
    let events = vec![retried(10, 2)];
    // TRIGGER PRESENT: the events give a retry in progress.
    assert_eq!(route_of(&events).route.retry, Some((2, 4)));
    let row = first_row(&on_screen(events));
    assert!(
        row.contains("active \u{b7} provider error, try 2 of 4 \u{b7} 12m40s"),
        "{row}"
    );
    let row = first_row(&on_screen(vec![retried(10, 2), turn_started(11)]));
    assert!(!row.contains("provider error"), "{row}");
}

#[test]
fn a_wait_and_an_outage_show_in_the_header() {
    let events = vec![waited(10), outage(11)];
    let v = route_of(&events);
    assert!(v.route.waiting && v.route.outage_minutes == Some(30));
    let row = first_row(&on_screen(events));
    assert!(
        row.contains(
            "active \u{b7} waiting on the model provider; every model route down for 30 min"
        ),
        "{row}"
    );
}

#[test]
fn a_switch_alone_adds_no_trouble_to_the_header() {
    let row = first_row(&on_screen(vec![switched(10, "model", MODEL, SOL)]));
    assert!(
        row.contains("GPT-6.1 Sol \u{b7} active \u{b7} 12m40s"),
        "{row}"
    );
    assert!(!row.contains("running on"), "{row}");
}

#[test]
fn the_status_block_names_the_route_the_run_is_on_and_keeps_the_model_the_user_chose() {
    let events = vec![switched(10, "model", MODEL, SOL), retried(11, 2)];
    assert_eq!(route_of(&events).route.model.as_deref(), Some(SOL));
    let app = on_screen(events);
    let text = screen(&app);
    let lines: Vec<&str> = text.lines().collect();
    let model = lines
        .iter()
        .position(|l| l.contains("Model  Claude Sonnet 5.5"))
        .unwrap_or_else(|| panic!("the block lost the model the user chose:\n{text}"));
    assert!(
        lines[model + 1]
            .contains("Route  running on GPT-6.1 Sol after 1 switch; provider error, try 2 of 4"),
        "{text}"
    );
}

#[test]
fn a_run_on_its_own_model_has_no_route_row() {
    let text = screen(&on_screen(vec![]));
    assert!(text.contains("Model  Claude Sonnet 5.5"), "{text}");
    assert!(!has_route_row(&text), "{text}");
}

#[test]
fn a_switch_to_another_route_of_the_same_model_keeps_the_model_name() {
    let events = vec![switched(10, "route", MODEL, SONNET_US)];
    assert_eq!(route_of(&events).route.model.as_deref(), Some(SONNET_US));
    let app = on_screen(events);
    let row = first_row(&app);
    assert!(row.contains("Claude Sonnet 5.5"), "{row}");
    let route = block_row(&app, "Route  ");
    assert!(
        route.contains(
            "Route  running on Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5) after 1 switch"
        ),
        "{route}"
    );
}

#[test]
fn a_return_to_the_model_the_user_chose_puts_its_name_back_in_the_header() {
    let events = vec![
        switched(10, "model", MODEL, SOL),
        switched(11, "return", SOL, MODEL),
    ];
    assert_eq!(route_of(&events).route.model.as_deref(), Some(MODEL));
    let row = first_row(&on_screen(events));
    assert!(
        row.contains("Claude Sonnet 5.5") && !row.contains("GPT-6.1 Sol"),
        "{row}"
    );
}

#[test]
fn a_fresh_session_puts_the_header_back_on_the_model_the_user_chose() {
    let fresh = env(
        12,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "new-session", "text": "A fresh Cline session started.", "requestId": null,
               "session": 2, "trigger": "harness", "reason": "engine_stop", "restartedProcess": false,
               "digestBytes": 1200, "promptBytes": 1400}),
    );
    let on_sol = vec![switched(10, "model", MODEL, SOL)];
    assert!(
        first_row(&on_screen(on_sol.clone())).contains("GPT-6.1 Sol"),
        "TRIGGER ABSENT: the run is not on Sol before the fresh session"
    );
    let mut events = on_sol;
    events.push(fresh);
    let app = on_screen(events);
    let row = first_row(&app);
    assert!(
        row.contains("Claude Sonnet 5.5") && !row.contains("GPT-6.1 Sol"),
        "{row}"
    );
    assert!(!has_route_row(&screen(&app)), "{}", screen(&app));
}

#[test]
fn a_run_that_ended_shows_the_model_it_ended_on_and_no_live_route_or_trouble() {
    let mut events = vec![switched(10, "model", MODEL, SOL), retried(11, 2)];
    events.extend([
        env(
            12,
            EventType::GoalStatus,
            None,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        at(
            "2026-10-14T20:30:00Z",
            13,
            EventType::RunCompleted,
            None,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]);
    // TRIGGER PRESENT: the run is over and a retry was the last thing the stream said about the model calls.
    let v = route_of(&events);
    assert!(v.completed && v.route.retry.is_some());
    let app = on_screen(events);
    let row = first_row(&app);
    assert!(row.contains("GPT-6.1 Sol"), "{row}");
    assert!(!row.contains("provider error"), "{row}");
    assert!(!has_route_row(&screen(&app)), "{}", screen(&app));
}

/// The version tag takes the last 26 columns of the top row (`tui_version_tag.rs`), so the header that used to fit 80 columns now has the room
/// of a 100-column terminal: the trouble is kept and the tail (the age of the last read) goes first, as before.
#[test]
fn at_100_columns_the_header_keeps_the_trouble_and_gives_up_its_tail() {
    let app = on_screen(vec![retried(10, 2)]);
    let text = screen_at(&app, 100, 24);
    let row = text.lines().next().unwrap();
    assert!(row.chars().count() <= 100, "{row}");
    assert!(
        row.contains("active \u{b7} provider error, try 2 of 4"),
        "{row}"
    );
    assert!(!row.contains("updated"), "{row}");
}

/// At 80 columns the header has 54 of them beside the tag: the run id and the state stay whole, the trouble is cut, and the status card
/// below still says what happened.
#[test]
fn at_80_columns_the_header_keeps_the_run_id_and_the_state_beside_the_tag() {
    let app = on_screen(vec![retried(10, 2)]);
    let text = screen_at(&app, 80, 24);
    let row = text.lines().next().unwrap();
    assert!(row.chars().count() <= 80, "{row}");
    let (title, tag) = ham::tui::version_tag::split_tag(row);
    assert!(tag.is_some(), "{row}");
    assert!(
        title.starts_with(&format!("ham \u{b7} Run {RUN} \u{b7} active")),
        "{row}"
    );
    assert!(!title.contains("updated"), "{row}");
}
