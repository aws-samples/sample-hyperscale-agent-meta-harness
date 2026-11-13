//! The goal loop on the run screen: the lines agentd's goal module
//! writes show in the conversation as notices: the loop is on; one round counter that counts; the continue that was sent; the call of
//! Cline's goal-complete tool; the brake that ended the run with the engine's BLOCKED line apart and bold; three dim notes. No line calls
//! the work verified, tested, gated or evaluated, the label of the run is the one of `run.completed`, and the question card is unchanged.
#![allow(clippy::unwrap_used)]

use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn draw(app: &App, w: u16, h: u16) -> (String, Terminal<TestBackend>) {
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
    (out, t)
}

fn screen(app: &App) -> String {
    draw(app, 120, 40).0
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

fn detail() -> Msg {
    let d: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z",
                    "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with tests", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    Msg::Detail(Ok(d))
}

fn start() -> Vec<Envelope> {
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

fn goal(n: u32, phase: &str, extra: Value) -> Envelope {
    let mut v = json!({"turn": 1, "kind": "goal-loop", "phase": phase});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    env(n, EventType::TurnProgress, Some("direct"), v)
}

fn on_screen(events: Vec<Envelope>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let mut all = start();
    all.extend(events);
    app.update(pages(all));
    app.update(detail());
    assert_eq!(app.mode(), Mode::Browse);
    app
}

fn row_with<'a>(text: &'a str, needle: &str) -> Option<(u16, &'a str)> {
    text.lines()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .map(|(y, l)| (y as u16, l))
}

fn loop_events() -> Vec<Envelope> {
    vec![
        goal(
            10,
            "on",
            json!({"text": "Goal loop on: the engine checks the goal after each round."}),
        ),
        goal(11, "round", json!({"rounds": 1, "status": "completed"})),
        goal(12, "kick", json!({})),
        goal(13, "round", json!({"rounds": 2, "status": "completed"})),
        goal(
            14,
            "done",
            json!({"summary": "Added percent_change and its tests."}),
        ),
    ]
}

#[test]
fn the_goal_loop_lines_show_in_the_conversation_in_order() {
    let app = on_screen(loop_events());
    let text = screen(&app);
    let on = row_with(
        &text,
        "Goal loop on: the engine checks the goal after each round.",
    )
    .unwrap_or_else(|| panic!("no on note:\n{text}"))
    .0;
    let rounds = row_with(&text, "2 rounds ended, the last one completed.")
        .unwrap()
        .0;
    let kick = row_with(&text, "Goal loop: continue sent.").unwrap().0;
    let done = row_with(&text, "Cline called the goal-complete tool.")
        .unwrap()
        .0;
    assert!(on < rounds && rounds < kick && kick < done, "{text}");
    assert!(
        row_with(
            &text,
            "Cline's summary: Added percent_change and its tests."
        )
        .is_some(),
        "{text}"
    );
    // The notes wear the harness's word in the gutter.
    assert!(row_with(&text, "notice").is_some());
    let (_, line) = row_with(&text, "Goal loop: continue sent.").unwrap();
    assert!(line.starts_with("notice"), "{line:?}");
}

#[test]
fn the_round_counter_is_one_line_that_counts() {
    let mut app = on_screen(vec![goal(
        10,
        "round",
        json!({"rounds": 1, "status": "completed"}),
    )]);
    assert!(screen(&app).contains("1 round ended, the last one completed."));
    app.update(pages(vec![
        goal(11, "round", json!({"rounds": 2, "status": "completed"})),
        goal(12, "round", json!({"rounds": 3, "status": "failed"})),
    ]));
    let text = screen(&app);
    assert_eq!(text.matches("rounds ended").count(), 1, "{text}");
    assert!(
        text.contains("3 rounds ended, the last one failed."),
        "{text}"
    );
    assert!(!text.contains("1 round ended"), "{text}");
}

#[test]
fn the_brake_and_the_blocked_line_are_two_lines_and_the_blocked_one_is_bold() {
    let app = on_screen(vec![goal(
        10,
        "brake",
        json!({"rule": "blocked-twice",
               "text": "The harness ended the run: the engine reported BLOCKED twice. Its words: BLOCKED: the stage has no network"}),
    )]);
    let (text, t) = draw(&app, 120, 40);
    let (by, brake) = row_with(
        &text,
        "The harness ended the run: the engine reported BLOCKED twice.",
    )
    .unwrap();
    assert!(!brake.contains("Its words"), "{brake:?}");
    let (ly, line) = row_with(&text, "BLOCKED: the stage has no network").unwrap();
    assert!(ly > by, "{text}");
    // The line is bold from its first character.
    let x = line.find("BLOCKED: the").unwrap();
    let col = line[..x].chars().count() as u16;
    let cell = &t.backend().buffer()[(col, ly)];
    assert!(cell.modifier.contains(Modifier::BOLD), "{cell:?}\n{text}");
    // The brake line itself is not bold.
    let bx = brake.find("The harness").unwrap();
    let bcol = brake[..bx].chars().count() as u16;
    assert!(
        !t.backend().buffer()[(bcol, by)]
            .modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn the_three_quiet_notes_are_dim() {
    let app = on_screen(vec![
        goal(
            10,
            "off",
            json!({"text": "Goal loop off: the stage switched it off."}),
        ),
        goal(
            11,
            "fallback",
            json!({"text": "Goal loop: the round log is missing; the loop is watched without it."}),
        ),
        goal(
            12,
            "rearm",
            json!({"text": "Goal loop: the goal is set again for the new session."}),
        ),
    ]);
    let (text, t) = draw(&app, 120, 40);
    for needle in [
        "Goal loop off: the stage switched it off.",
        "the round log is missing",
        "the goal is set again for the new session.",
    ] {
        let (y, line) = row_with(&text, needle).unwrap_or_else(|| panic!("{needle}\n{text}"));
        let x = line.find(needle).unwrap();
        let col = line[..x].chars().count() as u16;
        let cell = &t.backend().buffer()[(col, y)];
        assert_eq!(cell.fg, Color::DarkGray, "{needle}: {cell:?}");
    }
    // The ordinary notes are not dim.
    let app = on_screen(vec![goal(10, "kick", json!({}))]);
    let (text, t) = draw(&app, 120, 40);
    let (y, line) = row_with(&text, "continue sent").unwrap();
    let col = line.find("Goal loop").unwrap() as u16;
    assert_ne!(t.backend().buffer()[(col, y)].fg, Color::DarkGray);
}

#[test]
fn no_goal_line_claims_that_the_work_was_checked() {
    let mut events = loop_events();
    events.push(goal(
        20,
        "brake",
        json!({"rule": "no-progress", "text": "The harness ended the run: no progress in the last rounds."}),
    ));
    events.push(goal(
        21,
        "off",
        json!({"text": "Goal loop off: switched off."}),
    ));
    let text = screen(&on_screen(events));
    for line in text
        .lines()
        .filter(|l| l.contains("Goal loop") || l.contains("The harness ended"))
    {
        // Cline's own summary is Cline's words; the harness's words carry none of these.
        if line.contains("Cline's summary:") {
            continue;
        }
        let lower = line.to_lowercase();
        for w in ["verified", "tested", "gated", "evaluated"] {
            assert!(!lower.contains(w), "{line:?} holds {w:?}");
        }
    }
}

#[test]
fn the_label_of_a_braked_run_is_the_one_run_completed_wrote() {
    let label = "Cline stopped: the harness ended the run (no progress in the last rounds)";
    let app = on_screen(vec![
        goal(
            10,
            "brake",
            json!({"rule": "no-progress", "text": "The harness ended the run: no progress in the last rounds."}),
        ),
        env(
            11,
            EventType::RunCompleted,
            None,
            json!({"status": "cleared", "outcome": "stopped_other", "label": label,
                   "detail": "The harness ended the run: no progress in the last rounds."}),
        ),
    ]);
    let text = screen(&app);
    assert!(
        text.contains(&format!("Outcome   {label}")) || text.contains(&format!("Outcome {label}")),
        "{text}"
    );
}

#[test]
fn the_question_card_of_the_recovery_ladder_is_unchanged_beside_the_goal_lines() {
    let question = env(
        30,
        EventType::TurnProgress,
        Some("direct"),
        json!({
            "turn": 1, "kind": "needs-input",
            "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
            "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
            "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": "x", "runS": 412, "sessions": 1},
            "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
            "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-14T20:30:00Z"
        }),
    );
    let mut events = loop_events();
    events.push(question);
    let text = screen(&on_screen(events));
    assert!(text.contains("Cline needs you"), "{text}");
    assert!(text.contains("p message"), "{text}");
    assert!(text.contains("2 rounds ended"), "{text}");
}
