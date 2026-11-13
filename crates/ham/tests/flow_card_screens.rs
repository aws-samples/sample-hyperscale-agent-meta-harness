//! On the screens of `ham`: the flow a run declared (`quick`, `full` or `none`) is a row of the delivery card of the terminal app and of
//! `ham watch`, read once from `evidence/direct.json` of the zip that was downloaded. The row is a fact beside the label: it changes no label
//! and no other row, and a zip with no record shows nothing more than before. The deliveries are fakes.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::Api;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Msg};
use ham::watch::{WatchOptions, watch};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage};
use harness_protocol::direct::{DirectOutcome, banned_claim_word};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const RUN: &str = "01M5HZD83QRNP7H7NN9S4KYJK3";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;

/// A delivered zip: `(path under the top directory, body)` entries.
fn zip_with(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(format!("calc-{RUN}-20f65bb/{name}"), opts)
            .unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// `evidence/direct.json` as run-session writes it: its own members and agentd's summary under `summary`.
fn evidence(flow: Option<&str>) -> Vec<u8> {
    let mut summary = json!({"schemaVersion": 1, "runId": RUN, "clineMessage": "Done."});
    if let Some(word) = flow {
        summary["flow"] = json!(word);
    }
    serde_json::to_vec(&json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done",
        "summary": summary
    }))
    .unwrap()
}

fn zip_of(flow: Option<&str>) -> Vec<u8> {
    zip_with(&[
        ("src/a.py", b"x".to_vec()),
        ("evidence/direct.json", evidence(flow)),
    ])
}

/// The screen as one string of words: the card's borders and the line breaks of its wrapping do not matter to what is said.
fn words(text: &str) -> String {
    text.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn env(n: u32, t: EventType, authority: Authority, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
        seq: u64::from(n),
        ts: "2026-10-27T14:58:30Z".into(),
        event_type: t,
        authority,
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

fn screen(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn finished() -> Vec<Envelope> {
    vec![
        env(
            1,
            EventType::RunCreated,
            Authority::Harness,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            Authority::Harness,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
        env(
            10,
            EventType::GoalStatus,
            Authority::Harness,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        env(
            11,
            EventType::DeliveryReady,
            Authority::Harness,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(
            12,
            EventType::RunCompleted,
            Authority::Harness,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]
}

fn downloaded(path: PathBuf) -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(ham::pull::Pulled {
            path,
            artifact: ArtifactView {
                artifact_id: "01K6A00000000000000000ZP01".into(),
                kind: ArtifactKind::Zip,
                s3_uri: format!("s3://b/runs/{RUN}/deliverable/calc.zip"),
                sha256: "ab".repeat(32),
                size_bytes: 2048,
            },
            url: DownloadUrlResponse {
                url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/calc.zip?X-Amz-Expires=900"
                    .into(),
                expires_at: "2026-10-27T20:15:00Z".into(),
            },
            ranges: vec![(0, 2047)],
            resumed_from: 0,
        }),
    }
}

fn delivered_app(zip: &[u8], label: &str) -> App {
    let dir = support::scratch(label);
    let path = dir.join("calc.zip");
    std::fs::write(&path, zip).unwrap();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(finished()));
    app.update(downloaded(path));
    app
}

// ---- the terminal app --------------------------------------------------------------------------------------------------------

#[test]
fn the_delivery_card_of_the_terminal_app_has_the_flow_row_of_the_zip() {
    for word in ["quick", "full", "none"] {
        let app = delivered_app(&zip_of(Some(word)), "flow-card-tui");
        let text = words(&screen(&app, 160, 50));
        assert!(
            text.contains(&format!("Flow {word}")),
            "the card says the flow:\n{}",
            screen(&app, 160, 50)
        );
        // The row sits beside the label and never changes it.
        assert!(text.contains("Cline reported done"), "{text}");
        assert_eq!(banned_claim_word(&text), None, "{text}");
    }
}

#[test]
fn a_zip_without_the_record_has_no_flow_row_and_nothing_moves() {
    let app = delivered_app(&zip_of(None), "flow-card-old");
    assert!(
        !screen(&app, 160, 50).contains("Flow "),
        "{}",
        screen(&app, 160, 50)
    );
    let app = delivered_app(
        &zip_with(&[("src/a.py", b"x".to_vec())]),
        "flow-card-none-record",
    );
    assert!(!screen(&app, 160, 50).contains("Flow "));
}

#[test]
fn before_the_zip_is_downloaded_there_is_no_flow_row() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(pages(finished()));
    assert!(!screen(&app, 160, 50).contains("Flow "));
}

// ---- `ham watch` ------------------------------------------------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
}

async fn new_run(fake: &Fake) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "flow card", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, "k-flow-card")
        .await
        .unwrap()
        .run_id
}

async fn watched(label: &str, zip: Vec<u8>) -> String {
    let fake = Fake::start().await;
    let run = new_run(&fake).await;
    fake.mem.inner.lock().unwrap().deliverable = Some(zip);
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let opts = WatchOptions {
        interval: Duration::from_millis(20),
        json: false,
        download_dir: Some(support::scratch(label)),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    watch(&api(&fake), &run, &opts, &mut out, &mut err)
        .await
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_final_card_of_ham_watch_has_the_flow_row_of_the_zip_it_downloaded() {
    let text = watched("flow-card-watch", zip_of(Some("full"))).await;
    assert_eq!(
        text.lines().filter(|l| *l == "Flow      full").count(),
        1,
        "{text}"
    );
    assert!(
        text.lines().any(|l| l == "Outcome   Cline reported done"),
        "the row does not touch the label: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ham_watch_without_the_record_in_the_zip_prints_what_it_printed_before() {
    let text = watched("flow-card-watch-none", zip_of(None)).await;
    assert!(!text.contains("Flow "), "{text}");
}
