//! The skills of a run on the run screen: when agentd placed the run's skills, the chat view shows a folded card after the
//! agent table, `skills · N placed`, which Space opens to one row per skill (name, the first 12 digits of its digest, files and bytes); the
//! conversation holds one plain notice instead of "Event skills". A run without skills has neither. No line says the work was verified,
//! tested, gated or evaluated.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn draw(app: &App, w: u16, h: u16) -> String {
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

fn screen(app: &App) -> String {
    draw(app, 120, 40)
}

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-10-17T20:00:00Z".into(),
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
                    "createdAt": "2026-10-17T20:00:00Z",
                    "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API with a few checks", "tokensUsed": 0, "costUsd": 0.0
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

fn digest(lead: char) -> String {
    std::iter::repeat_n(lead, 12)
        .chain(std::iter::repeat_n('0', 52))
        .collect()
}

fn skills(n: u32, ms: Option<u64>) -> Envelope {
    env(
        n,
        EventType::TurnProgress,
        Some("direct"),
        json!({"turn": 1, "kind": "skills",
               "skills": [
                   {"name": "lifecycle-research", "digest": digest('a'), "files": 1, "bytes": 1203},
                   {"name": "lifecycle-check", "digest": digest('b'), "files": 2, "bytes": 18940},
                   {"name": "my-skill", "digest": digest('c'), "files": 1, "bytes": 87}],
               "preflight": {"ran": ms.is_some(), "ms": ms}}),
    )
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

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

/// Opens the skills card: a draw first (the cards are set on the chat view by a draw), then Shift-Tab selects the last block, which is that
/// card when the run has not been delivered, and Space opens it.
fn open_the_card(app: &mut App) {
    let _ = screen(app);
    press(app, KeyCode::BackTab);
    press(app, KeyCode::Char(' '));
}

#[test]
fn a_run_that_placed_skills_shows_a_folded_card_with_the_count() {
    let app = on_screen(vec![skills(10, Some(82))]);
    let text = screen(&app);
    assert!(text.contains("skills · 3 placed"), "no card title:\n{text}");
    // Folded: the rows are not drawn until the card is opened.
    assert!(!text.contains("sha256 aaaaaaaaaaaa"), "{text}");
    println!("TUI-SKILLS-CARD TRIGGER PRESENT: folded card 'skills · 3 placed' on the run screen");
}

#[test]
fn space_opens_the_card_to_one_row_per_skill_with_a_short_digest() {
    let mut app = on_screen(vec![skills(10, Some(82))]);
    open_the_card(&mut app);
    let text = screen(&app);
    for want in [
        "lifecycle-research",
        "sha256 aaaaaaaaaaaa",
        "lifecycle-check",
        "sha256 bbbbbbbbbbbb",
        "my-skill",
        "sha256 cccccccccccc",
        "2 files, 18,940 bytes",
        "listed all 3",
        "82 ms",
    ] {
        assert!(
            text.contains(want),
            "{want:?} is not on the screen:\n{text}"
        );
    }
    assert!(
        !text.contains(&digest('a')),
        "only the short digest is drawn:\n{text}"
    );
    let card: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.contains("skills · 3 placed"))
        .take(8)
        .collect();
    println!("TUI-SKILLS-CARD OPEN CARD:\n{}", card.join("\n"));
}

#[test]
fn the_card_fits_a_small_terminal() {
    let mut app = on_screen(vec![skills(10, None)]);
    open_the_card(&mut app);
    let text = draw(&app, 80, 30);
    assert!(text.contains("skills · 3 placed"), "{text}");
    assert!(text.contains("lifecycle-research"), "{text}");
    assert!(text.contains("not available"), "{text}");
}

#[test]
fn a_run_without_skills_has_no_skills_card() {
    let app = on_screen(vec![]);
    let text = screen(&app);
    assert!(!text.contains("skills ·"), "{text}");
    assert!(!text.contains("sha256"), "{text}");
}

#[test]
fn the_conversation_has_one_plain_notice_for_the_skills_event() {
    let app = on_screen(vec![skills(10, Some(82))]);
    let text = screen(&app);
    assert!(
        text.contains("Skills: 3 placed (lifecycle-research, lifecycle-check, my-skill)."),
        "{text}"
    );
    assert!(!text.contains("Event skills"), "{text}");
}

#[test]
fn nothing_on_the_card_says_the_work_was_checked() {
    let mut app = on_screen(vec![skills(10, Some(82))]);
    open_the_card(&mut app);
    let card: Vec<String> = screen(&app)
        .lines()
        .skip_while(|l| !l.contains("skills · 3 placed"))
        .take(12)
        .map(str::to_lowercase)
        .collect();
    assert!(card.len() >= 5, "{card:?}");
    for l in &card {
        for w in ["verified", "tested", "gated", "evaluated"] {
            assert!(!l.contains(w), "{l:?}");
        }
    }
}
