//! The dashboard of a direct run: once the harness has written `run.completed`, the status
//! block and the delivery card name the label it carried, "Cline reported done" or "Cline stopped: <reason>", exactly as delivered;
//! the old line's gate counts, gates row and Evidence block are not drawn for a direct run; and no screen says the harness verified,
//! tested, gated or evaluated anything. Screens are pinned by insta snapshots at 80x24 and 100x30.
#![allow(clippy::unwrap_used)]

mod support;

use ham::tui::{App, Msg};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage};
use harness_protocol::direct::{DirectOutcome, banned_claim_word};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::PathBuf;
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

const SIZES: [(u16, u16); 2] = [(80, 24), (100, 30)];
const RUN: &str = "01K6A0000000000000000000R1";

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
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

fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: 0,
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

fn downloaded() -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(ham::pull::Pulled {
        path: PathBuf::from("/downloads/python-calc-R1-abc1234.zip"),
        artifact: ArtifactView {
            artifact_id: "01K6A00000000000000000ZP01".into(),
            kind: ArtifactKind::Zip,
            s3_uri: "s3://b/runs/R1/deliverable/python-calc-R1-abc1234.zip".into(),
            sha256: "ab".repeat(32),
            size_bytes: 2048,
        },
        url: DownloadUrlResponse {
            url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/deliverable/python-calc-R1-abc1234.zip?X-Amz-Expires=900".into(),
            expires_at: "2026-10-14T20:15:00Z".into(),
        },
        ranges: vec![(0, 2047)],
        resumed_from: 0,
    }),
    }
}

/// A direct run that delivered: its one session, the pull request, the zip and `run.completed` with the outcome.
fn delivered(outcome: DirectOutcome, detail: Option<&str>) -> Vec<Envelope> {
    let mut done =
        json!({"status": "complete", "outcome": outcome.as_str(), "label": outcome.label()});
    if let Some(d) = detail {
        done["detail"] = json!(d);
    }
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
        env(
            4,
            EventType::TurnEnded,
            Some("direct"),
            json!({"summary": "added percent_change and its tests"}),
        ),
        env(
            5,
            EventType::DeliveryPr,
            None,
            json!({"url": "https://example.test/pr/7"}),
        ),
        env(
            6,
            EventType::GoalStatus,
            None,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        env(
            7,
            EventType::DeliveryReady,
            None,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(8, EventType::RunCompleted, None, done),
    ]
}

fn open(events: Vec<Envelope>, with_zip: bool) -> App {
    let mut app = App::new_direct("n".into(), None, ham::tui::DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(events));
    if with_zip {
        app.update(downloaded());
    }
    app
}

fn assert_clean(app: &App, label: &str) {
    for (w, h) in [(80, 24), (100, 30), (120, 40), (200, 60)] {
        let text = screen_text(app, w, h);
        assert_eq!(banned_claim_word(&text), None, "{w}x{h}:\n{text}");
        for absent in ["Evidence", "gates", "coverage", "findings", "verdicts"] {
            assert!(
                !text.contains(absent),
                "a direct run draws no {absent:?} at {w}x{h}:\n{text}"
            );
        }
        // The label is cut to the width only by the terminal: it appears whole in the card, wrapped over lines when narrow.
        let squashed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let words: Vec<&str> = label.split_whitespace().collect();
        assert!(
            words.iter().all(|w| squashed.contains(w)),
            "{w}x{h} lacks {label:?}:\n{text}"
        );
    }
}

#[test]
fn a_run_that_reported_done_shows_the_label_in_the_status_block_and_the_card() {
    let app = open(delivered(DirectOutcome::ReportedDone, None), true);
    let text = screen_text(&app, 100, 30);
    // The status block and the card name the model.
    assert!(text.contains("Model  Claude Sonnet 5.5"), "{text}");
    assert!(
        text.contains("Model     Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)"),
        "{text}"
    );
    assert!(text.contains("Outcome   Cline reported done"), "{text}");
    // The status block says it, and so does the delivery card below it.
    assert!(text.contains("Outcome Cline reported done"), "{text}");
    assert_clean(&app, "Cline reported done");
    snapshot_sizes(&app, "direct_dash_done");
}

/// `GET /v1/runs/{id}` for a direct run: no goal counter (`tokensUsed` is 0), and a `cost` priced from the usage its turn recorded
/// (100,000 input, 100,000 output and 1,000,000 cache read tokens of claude-sonnet-5: 1.54 USD).
fn run_detail(with_cost: bool) -> Msg {
    let mut v = json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "delivering", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": support::ME},
        "tokensUsed": 0, "costUsd": if with_cost { 1.54 } else { 0.0 }
    });
    if with_cost {
        v["cost"] = json!({"pricesVersion": "2026-10-01", "turnsWithoutUsage": 0, "lines": [{
            "modelId": MODEL, "engine": "cline", "usd": 1.54,
            "usage": {"inputTokens": 100_000, "outputTokens": 100_000,
                      "cacheReadInputTokens": 1_000_000, "cacheWriteInputTokens": 0}}]});
    }
    Msg::Detail(Ok(serde_json::from_value(v).unwrap()))
}

/// The stage reported no usage for the turn (it ended without engine-reported usage): the card has no Tokens line, not a "0  USD 0.00".
#[test]
fn a_run_with_no_reported_usage_draws_no_tokens_line() {
    let mut app = open(delivered(DirectOutcome::ReportedDone, None), false);
    app.update(run_detail(false));
    app.update(downloaded());
    for (w, h) in [(80, 24), (100, 30), (120, 40), (200, 60)] {
        let text = screen_text(&app, w, h);
        assert!(
            text.contains("Outcome Cline reported done"),
            "{w}x{h}:\n{text}"
        );
        assert!(
            !text.contains("Tokens") && !text.contains("USD"),
            "{w}x{h}:\n{text}"
        );
    }
    // The same when the detail never arrived.
    let app = open(delivered(DirectOutcome::ReportedDone, None), true);
    let text = screen_text(&app, 100, 30);
    assert!(!text.contains("Tokens") && !text.contains("USD"), "{text}");
}

/// The tokens are the sum of the usage in the cost lines and the cost is the stage's price for them.
#[test]
fn a_run_with_reported_usage_draws_the_tokens_and_the_cost() {
    let mut app = open(delivered(DirectOutcome::ReportedDone, None), false);
    app.update(run_detail(true));
    app.update(downloaded());
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("Tokens    1200000  USD 1.54"), "{text}");
    assert_clean(&app, "Cline reported done");
    snapshot_sizes(&app, "direct_dash_usage");
}

#[test]
fn a_run_cline_stopped_halfway_shows_the_stop_and_its_detail() {
    let app = open(
        delivered(
            DirectOutcome::StoppedSilence,
            Some("no output for 900 s; pull request not opened: push refused"),
        ),
        true,
    );
    let text = screen_text(&app, 100, 30);
    assert!(
        text.contains("Outcome   Cline stopped: no output for the silence limit"),
        "{text}"
    );
    assert!(text.contains("Detail    no output for 900 s"), "{text}");
    assert_clean(&app, "Cline stopped: no output for the silence limit");
    snapshot_sizes(&app, "direct_dash_stopped");
}

#[test]
fn every_outcome_of_the_stop_table_is_drawn_with_its_own_label() {
    for outcome in DirectOutcome::ALL {
        let app = open(delivered(outcome, None), true);
        let text = screen_text(&app, 120, 40);
        assert!(
            text.contains(&format!("Outcome   {}", outcome.label())),
            "{outcome:?}:\n{text}"
        );
        assert_clean(&app, outcome.label());
    }
}

#[test]
fn a_blocked_direct_run_shows_why_and_the_label_and_no_gates() {
    let events = vec![
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
            EventType::GoalBlocked,
            None,
            json!({"reason": "the session ended before any work was saved"}),
        ),
        env(
            4,
            EventType::GoalStatus,
            None,
            json!({"status": "blocked", "phase": "delivering"}),
        ),
        env(
            5,
            EventType::RunCompleted,
            None,
            json!({"status": "blocked", "outcome": "stopped_session_lost",
                "label": DirectOutcome::StoppedSessionLost.label()}),
        ),
    ];
    let app = open(events, false);
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("blocked (delivering)"), "{text}");
    assert!(
        text.contains(&format!(
            "Outcome {}",
            DirectOutcome::StoppedSessionLost.label()
        )),
        "{text}"
    );
    assert!(
        text.contains("the session ended before any work was saved"),
        "{text}"
    );
    assert_clean(&app, DirectOutcome::StoppedSessionLost.label());
    snapshot_sizes(&app, "direct_dash_blocked");
}

/// The run asked for Claude Sonnet 5.5 and ended on GLM 5.3.
const A12_DETAIL: &str = "Ran on us.zai.glm-5.3 after 9 switches. the engine ended the prompt cancelled with no cancel request (mistake limit or a cut model stream)";

/// A run that switched models: the status block (near the top) says where it ended and keeps the model it asked for; the delivery card
/// shows the model it ended on in its Model row, the requested model on the next row, and its Detail row as sentences with the display name.
#[test]
fn a_run_that_switched_models_shows_where_it_ended_in_the_status_block_and_the_card() {
    let app = open(
        delivered(DirectOutcome::StoppedEngineCancelled, Some(A12_DETAIL)),
        true,
    );
    for (w, h) in [(100, 30), (120, 40), (200, 60)] {
        let text = screen_text(&app, w, h);
        let lines: Vec<&str> = text.lines().collect();
        let model = lines
            .iter()
            .position(|l| l.contains("Model  Claude Sonnet 5.5"))
            .unwrap_or_else(|| panic!("{w}x{h}:\n{text}"));
        assert!(
            lines[model + 1].contains("Ran on GLM 5.3 after 9 switches"),
            "{w}x{h}: the status block says where the run ended:\n{text}"
        );
        assert!(!lines[model + 1].contains("Detail"), "{w}x{h}:\n{text}");
        let delivery = lines
            .iter()
            .position(|l| l.contains("delivery"))
            .unwrap_or_else(|| panic!("{w}x{h}:\n{text}"));
        assert!(model + 1 < delivery, "{w}x{h}:\n{text}");
        let card = lines[delivery..].join("\n");
        for line in [
            "Model     GLM 5.3 (us.zai.glm-5.3)",
            "Requested Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)",
            "Detail    Ran on GLM 5.3 after 9 switches. The engine ended",
        ] {
            assert!(card.contains(line), "{w}x{h} lacks {line:?}:\n{text}");
        }
        assert!(
            !text.contains("Ran on us.zai.glm-5.3"),
            "{w}x{h}: no raw model id in the sentence:\n{text}"
        );
    }
}

/// A run that did not switch draws no "Ran on" row and no Requested row.
#[test]
fn a_run_that_did_not_switch_draws_no_ran_on_row_and_no_requested_row() {
    let app = open(delivered(DirectOutcome::ReportedDone, None), true);
    let text = screen_text(&app, 120, 40);
    assert!(
        !text.contains("Ran on") && !text.contains("Requested"),
        "{text}"
    );
}
