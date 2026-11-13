//! A zip download answers on its own task, so its answer can arrive after the person opened another run. The answer names
//! the run it is for, and the dashboard of any other run ignores it, as it ignores a clone answer: it must not put one run's zip, or one
//! run's failure, on another run's card.
#![allow(clippy::unwrap_used)]

use ham::pull::Pulled;
use ham::tui::{App, DirectSettings, Effect, Msg};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::PathBuf;

const RUN_A: &str = "01K6A0000000000000000000R1";
const RUN_B: &str = "01K6A0000000000000000000R2";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

fn artifact_of(run: &str) -> String {
    format!("01K6A00000000000000000{}", &run[run.len() - 4..])
}

fn screen_text(app: &App) -> String {
    let (w, h) = (140, 40);
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
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
    out
}

fn env(run: &str, n: u32, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: run.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

/// The pages of a delivered run: created, one session, the delivery of its zip and `run.completed`.
fn delivered_pages(run: &str) -> Msg {
    let events = vec![
        env(
            run,
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            run,
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            run,
            6,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        env(
            run,
            7,
            EventType::DeliveryReady,
            json!({"artifactId": artifact_of(run), "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(
            run,
            8,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ];
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

fn pulled(run: &str) -> Pulled {
    let name = format!("python-calc-{run}-abc1234.zip");
    Pulled {
        path: PathBuf::from(format!("/downloads/{name}")),
        artifact: ArtifactView {
            artifact_id: artifact_of(run),
            kind: ArtifactKind::Zip,
            s3_uri: format!("s3://b/runs/{run}/deliverable/{name}"),
            sha256: "ab".repeat(32),
            size_bytes: 2048,
        },
        url: DownloadUrlResponse {
            url: format!("https://b.s3.us-west-2.amazonaws.com/runs/{run}/deliverable/{name}"),
            expires_at: "2026-10-14T20:15:00Z".into(),
        },
        ranges: vec![(0, 2047)],
        resumed_from: 0,
    }
}

/// Run A is delivered and its download starts; then the person opens run B, which is delivered too, and its download starts.
fn focus_moved_during_a_download() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN_A);
    let effects = app.update(delivered_pages(RUN_A));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Download { run_id, .. } if run_id == RUN_A
        )),
        "run A starts its download: {effects:?}"
    );
    app.open_run(RUN_B);
    let effects = app.update(delivered_pages(RUN_B));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Download { run_id, .. } if run_id == RUN_B
        )),
        "run B starts its own download: {effects:?}"
    );
    app
}

#[test]
fn a_download_answer_for_another_run_does_not_land_on_the_card() {
    let mut app = focus_moved_during_a_download();
    let before = screen_text(&app);
    assert!(before.contains("downloading the zip"), "{before}");
    // Run A's download ends while run B is on screen.
    app.update(Msg::Downloaded {
        run_id: RUN_A.into(),
        result: Ok(pulled(RUN_A)),
    });
    let text = screen_text(&app);
    assert!(
        !text.contains(&format!("python-calc-{RUN_A}-abc1234.zip")),
        "run A's zip is on run B's card:\n{text}"
    );
    assert!(
        text.contains("downloading the zip"),
        "run B is still downloading:\n{text}"
    );
    // Run B's own answer lands.
    app.update(Msg::Downloaded {
        run_id: RUN_B.into(),
        result: Ok(pulled(RUN_B)),
    });
    let text = screen_text(&app);
    assert!(
        text.contains(&format!("python-calc-{RUN_B}-abc1234.zip")),
        "{text}"
    );
}

#[test]
fn a_failed_download_of_another_run_does_not_fail_the_card() {
    let mut app = focus_moved_during_a_download();
    app.update(Msg::Downloaded {
        run_id: RUN_A.into(),
        result: Err("download: sha256 mismatch".into()),
    });
    let text = screen_text(&app);
    assert!(
        !text.contains("sha256 mismatch") && !text.contains("download failed"),
        "run A's failure is on run B's card:\n{text}"
    );
    assert!(text.contains("downloading the zip"), "{text}");
    // Run B's own failure shows.
    app.update(Msg::Downloaded {
        run_id: RUN_B.into(),
        result: Err("GET b/path: HTTP 403".into()),
    });
    let text = screen_text(&app);
    assert!(
        text.contains("download failed: GET b/path: HTTP 403"),
        "{text}"
    );
}
