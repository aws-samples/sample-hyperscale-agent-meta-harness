//! Terminal control sequences in event text never reach the terminal raw:
//! every screen is drawn through ratatui's crossterm backend into a byte buffer,
//! and the bytes may hold only the backend's own CSI sequences (cursor moves, colors, clears):
//! no OSC (clipboard writes, hyperlinks, window titles), DCS, APC, PM or SOS string, no bare ESC, no BEL or
//! other C0 control besides CR and LF, no C1 control and no bidirectional override.
#![allow(clippy::unwrap_used)]

use ham::tui::{App, Msg};
use harness_protocol::api::v1::{ArtifactView, EventsPage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use proptest::prelude::*;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::json;
use std::collections::BTreeMap;

const RUN: &str = "01K6A0000000000000000000R1";

/// Control sequences that the text an engine writes might hold.
const CONTROL_TEXT: [&str; 10] = [
    "\u{1b}]0;title\u{7}",
    "\u{1b}]8;;https://example.test\u{7}click\u{1b}]8;;\u{7}",
    "\u{1b}]0;window title\u{1b}\\",
    "\u{1b}P+q544e\u{1b}\\",
    "\u{1b}_apc\u{1b}\\",
    "\u{9b}31m\u{9d}0;title\u{9c}",
    "\u{1b}[2J\u{1b}[H\u{1b}[?1049h",
    "\u{7}\u{8}\u{b}\u{c}\u{7f}",
    "\u{202e}desrever\u{202c}",
    "\u{1b}c\u{1b}(0",
];

fn env(
    n: u32,
    t: EventType,
    task: Option<&str>,
    agent: Option<&str>,
    v: serde_json::Value,
) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A000000000000000{n:06}"),
        run_id: RUN.into(),
        task_id: task.map(str::to_string),
        agent_id: agent.map(str::to_string),
        seq: 1,
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

fn control_run(text: &str) -> Vec<Envelope> {
    let a = format!("A{text}");
    vec![
        env(0, EventType::RunCreated, None, None, json!({})),
        env(
            1,
            EventType::SessionStarted,
            None,
            None,
            json!({"agentId": a, "engine": format!("cline{text}"), "modelId": format!("m{text}")}),
        ),
        env(
            2,
            EventType::ToolSummary,
            None,
            Some(&a),
            json!({"toolCallId": "c1", "title": text, "kind": "execute", "status": "pending"}),
        ),
        env(
            4,
            EventType::TurnEnded,
            None,
            Some(&a),
            json!({"turn": 1, "outcome": text}),
        ),
        env(
            7,
            EventType::GoalBlocked,
            None,
            None,
            json!({"reason": text}),
        ),
        env(
            8,
            EventType::DeliveryPr,
            None,
            None,
            json!({"url": format!("https://x/{text}")}),
        ),
        env(
            9,
            EventType::RunCompleted,
            None,
            None,
            json!({"status": "blocked", "outcome": text, "label": text, "detail": text}),
        ),
    ]
}

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by: BTreeMap<u8, Vec<Envelope>> = BTreeMap::new();
    for e in events {
        by.entry(harness_protocol::store::event_shard(
            e.authority,
            e.agent_id.as_deref(),
        ))
        .or_default()
        .push(e);
    }
    Msg::Pages(Ok(by
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

/// A writer whose bytes the test keeps.
#[derive(Clone, Default)]
struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes the crossterm backend writes for one frame of `app`.
fn terminal_bytes(app: &App, w: u16, h: u16) -> Vec<u8> {
    let out = Shared::default();
    let mut t = Terminal::with_options(
        CrosstermBackend::new(out.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, w, h)),
        },
    )
    .unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    drop(t);
    out.0.lock().unwrap().clone()
}

/// Every ESC starts a CSI whose final byte is one the backend uses; no other control is present.
fn assert_only_backend_sequences(bytes: &[u8]) {
    let text = String::from_utf8(bytes.to_vec()).expect("the backend writes UTF-8");
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let n = c as u32;
        if c == '\u{1b}' {
            assert_eq!(
                chars.get(i + 1),
                Some(&'['),
                "a non-CSI escape at {i}: {:?}",
                &text[..]
            );
            let mut j = i + 2;
            while j < chars.len() && (0x20..=0x3f).contains(&(chars[j] as u32)) {
                j += 1;
            }
            let fin = chars.get(j).copied().unwrap_or('\0');
            assert!(
                matches!(
                    fin,
                    'H' | 'm' | 'J' | 'K' | 'h' | 'l' | 'G' | 'A' | 'B' | 'C' | 'D'
                ),
                "CSI final {fin:?} at {i}"
            );
            i = j + 1;
            continue;
        }
        assert!(
            !(n < 0x20 && c != '\r' && c != '\n') && n != 0x7f && !(0x80..=0x9f).contains(&n),
            "control {n:#x} at {i}"
        );
        assert!(
            !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
            "bidi {n:#x}"
        );
        i += 1;
    }
}

#[test]
fn every_screen_with_control_sequences_writes_only_the_backends_sequences() {
    for text in CONTROL_TEXT {
        let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
        app.open_run(RUN);
        app.update(pages(control_run(text)));
        for (w, h) in [(80, 24), (200, 60)] {
            assert_only_backend_sequences(&terminal_bytes(&app, w, h));
        }
        app.update(Msg::Pages(Err(format!("network: {text}"))));
        assert_only_backend_sequences(&terminal_bytes(&app, 120, 40));
        // The Artifacts screen: artifact ids and keys come from the API.
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        app.update(Msg::Artifacts(Ok(vec![ArtifactView {
            artifact_id: format!("ID{text}"),
            kind: ArtifactKind::Zip,
            s3_uri: format!("s3://b/runs/{RUN}/deliverable/{text}.zip"),
            sha256: text.to_string(),
            size_bytes: 1,
        }])));
        assert_only_backend_sequences(&terminal_bytes(&app, 120, 40));
        app.update(Msg::Artifacts(Err(text.to_string())));
        assert_only_backend_sequences(&terminal_bytes(&app, 120, 40));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn any_agent_text_is_drawn_without_raw_controls(s in any::<String>()) {
        let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
        app.open_run(RUN);
        app.update(pages(control_run(&s)));
        assert_only_backend_sequences(&terminal_bytes(&app, 120, 40));
    }
}
