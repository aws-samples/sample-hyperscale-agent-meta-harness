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
use std::time::Instant;

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

fn snapshot_sizes(app: &App, name: &str) {
    for (w, h) in SIZES {
        insta::assert_snapshot!(format!("{name}_{w}x{h}"), screen_text(app, w, h));
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

#[test]
fn five_thousand_agents_draw_in_bounded_time() {
    let app = dashboard(agents(5000));
    let t = Instant::now();
    snapshot_sizes(&app, "dash_5000_agents");
    for _ in 0..10 {
        screen_text(&app, 200, 60);
    }
    let per_draw = t.elapsed().as_secs_f64() / 13.0;
    assert!(
        per_draw < 0.5,
        "a draw of 5,000 agents took {per_draw:.3} s"
    );
    assert!(screen_text(&app, 200, 60).contains("agents 5000"));
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
