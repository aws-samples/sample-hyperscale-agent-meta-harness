//! The dashboard's link keys.
//! Once a run is delivered the delivery card shows its download link as one
//! short line, never the 1,000-character presigned URL that wrapped over eight screen lines. `o` opens the pull request, else a
//! fresh download link, in the default browser (an argument vector, never a shell; `HAMH_OPEN_COMMAND` names the program), and `d`
//! downloads the zip again through the `ham pull` code path. No screen, notice or error carries a presigned URL.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::pull::Pulled;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Opened, perform};
use harness_protocol::api::v1::{ArtifactView, DownloadUrlResponse, EventsPage};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const SIZES: [(u16, u16); 3] = [(80, 24), (100, 30), (140, 40)];
const RUN: &str = "01K6A0000000000000000000R1";
const ARTIFACT: &str = "01K6A00000000000000000ZP01";
const MODEL: &str = ham_core::models::DEFAULT_MODEL_ID;
const OLD_FOOTER: &str =
    "↑↓ PgUp PgDn scroll · a artifacts · c cancel · Esc runs · n new run · q quit";

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    buffer_text(t.backend(), w, h)
}

fn buffer_text(b: &TestBackend, w: u16, h: u16) -> String {
    let buf = b.buffer().clone();
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

fn footer(text: &str) -> &str {
    text.lines().last().unwrap()
}

fn press(app: &mut App, c: char) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn env(n: u32, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: t,
        authority: Authority::Harness,
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

/// The run's events: created, one session, optionally the pull request, and (when `zip`) the delivery of the zip and `run.completed`.
fn events(pr: bool, zip: bool) -> Vec<Envelope> {
    let mut v = vec![
        env(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        ),
    ];
    if pr {
        v.push(env(
            5,
            EventType::DeliveryPr,
            json!({"url": "https://example.test/pr/7"}),
        ));
    }
    if zip {
        v.push(env(
            6,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        ));
        v.push(env(
            7,
            EventType::DeliveryReady,
            json!({"artifactId": ARTIFACT, "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ));
        v.push(env(
            8,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ));
    } else {
        v.push(env(
            3,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ));
    }
    v
}

/// A presigned URL as S3 makes them: long, with a security token and a signature in the query.
fn long_url() -> String {
    format!(
        "https://b.s3.us-west-2.amazonaws.com/runs/R1/deliverable/python-calc-R1-abc1234.zip?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=cred%2F20261014%2Fus-west-2%2Fs3&X-Amz-Date=20261014T200000Z&X-Amz-Expires=900&X-Amz-Security-Token={}&X-Amz-Signature={}",
        "TOKEN".repeat(140),
        "f".repeat(64)
    )
}

fn pulled_at(path: &str, expires: &str) -> Pulled {
    Pulled {
        path: PathBuf::from(path),
        artifact: ArtifactView {
            artifact_id: ARTIFACT.into(),
            kind: ArtifactKind::Zip,
            s3_uri: "s3://b/runs/R1/deliverable/python-calc-R1-abc1234.zip".into(),
            sha256: "ab".repeat(32),
            size_bytes: 2048,
        },
        url: DownloadUrlResponse {
            url: long_url(),
            expires_at: expires.into(),
        },
        ranges: vec![(0, 2047)],
        resumed_from: 0,
    }
}

fn downloaded() -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(pulled_at(
            "/downloads/python-calc-R1-abc1234.zip",
            "2026-10-14T20:15:00Z",
        )),
    }
}

fn app_with(opener: Option<&str>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_open_command(opener.map(str::to_string));
    app
}

/// A delivered run whose zip has downloaded.
fn delivered(pr: bool, opener: Option<&str>) -> App {
    let mut app = app_with(opener);
    app.open_run(RUN);
    app.update(pages(events(pr, true)));
    app.update(downloaded());
    app
}

fn working() -> App {
    let mut app = app_with(None);
    app.open_run(RUN);
    app.update(pages(events(false, false)));
    app
}

const NO_URL: [&str; 7] = [
    "X-Amz",
    "amazonaws.com",
    "Signature",
    "presign",
    "TOKENTOKEN",
    "Security-Token",
    "https://b.",
];

fn assert_no_url(text: &str, what: &str) {
    for bad in NO_URL {
        assert!(!text.contains(bad), "{what} shows {bad:?}:\n{text}");
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// The card

#[test]
fn the_delivery_card_shows_the_link_as_one_short_line_and_never_the_url() {
    let app = delivered(false, None);
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("Link      ")).collect();
        assert_eq!(rows.len(), 1, "{w}x{h}:\n{text}");
        let row = rows[0];
        assert!(row.contains("s3 · "), "{w}x{h}: {row}");
        assert!(row.contains("expires 20:15 UTC"), "{w}x{h}: {row}");
        assert!(row.contains("o open · d download"), "{w}x{h}: {row}");
        if w >= 100 {
            assert!(row.contains("python-calc-R1-abc1234.zip"), "{w}x{h}: {row}");
        }
        assert_no_url(&text, &format!("{w}x{h}"));
    }
}

/// The delivery card as the owner sees it, pinned at three sizes (a run with a pull request, and one without).
#[test]
fn the_delivery_card_snapshots() {
    let with_pr = delivered(true, None);
    let without = delivered(false, None);
    for (w, h) in SIZES {
        insta::assert_snapshot!(
            format!("links_card_pr_{w}x{h}"),
            screen_text(&with_pr, w, h)
        );
        insta::assert_snapshot!(
            format!("links_card_zip_only_{w}x{h}"),
            screen_text(&without, w, h)
        );
    }
}

/// Terminal hyperlinks (OSC 8) would make the short label clickable in terminals that support them, and they are not used: ratatui
/// drops the escape byte of every string it draws, so the rest of the sequence, including the whole URL, is drawn as text; and a
/// cell symbol that holds the sequence makes the buffer diff skip as many cells as the URL has characters. The `o` key is the
/// way to open a link.
#[test]
fn ratatui_drops_the_escape_byte_of_a_hyperlink_so_osc_8_is_not_used() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::widgets::Paragraph;
    let link = "\u{1b}]8;;https://example.test/pr/7\u{1b}\\Link\u{1b}]8;;\u{1b}\\";
    let mut t = Terminal::new(TestBackend::new(60, 1)).unwrap();
    t.draw(|f| f.render_widget(Paragraph::new(link), f.area()))
        .unwrap();
    let text = buffer_text(t.backend(), 60, 1);
    assert!(
        !text.contains('\u{1b}'),
        "the escape byte is dropped: {text:?}"
    );
    assert_eq!(
        text.trim_end(),
        "]8;;https://example.test/pr/7\\Link]8;;\\",
        "the rest is drawn as visible text"
    );
    // Put the sequence in one cell's symbol instead: the cells after it no longer reach the terminal.
    let area = Rect::new(0, 0, 60, 1);
    let before = Buffer::empty(area);
    let mut after = Buffer::empty(area);
    after[(0, 0)].set_symbol(link);
    after.set_string(1, 0, "abc", Style::default());
    let drawn: Vec<u16> = before.diff(&after).iter().map(|(x, _, _)| *x).collect();
    assert!(
        !drawn.contains(&1) && !drawn.contains(&2),
        "the cells after the hyperlink are skipped by the diff: {drawn:?}"
    );
}

#[test]
fn the_card_names_the_clone_and_push_commands() {
    let app = delivered(true, None);
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains(&format!("Clone     press C, or run: ham clone {RUN}")),
        "{text}"
    );
    assert!(
        text.contains(&format!("Push      ham push {RUN} <git-url>")),
        "{text}"
    );
    assert!(
        text.contains("PR        https://example.test/pr/7"),
        "{text}"
    );
}

/// With a pull request on the card, `o` opens that, so the Link line does not offer `o`.
#[test]
fn with_a_pull_request_the_link_line_offers_only_the_download_key() {
    let text = screen_text(&delivered(true, None), 140, 40);
    let row = text.lines().find(|l| l.contains("Link      ")).unwrap();
    assert!(
        row.contains("d download") && !row.contains("o open"),
        "{row}"
    );
}

#[test]
fn the_expiry_is_written_by_the_clock_the_app_was_given_and_lapses_with_the_wall_clock() {
    let mut app = delivered(false, None);
    app.set_clock(|_| "06:12 MDT".to_string());
    let text = screen_text(&app, 140, 40);
    assert!(text.contains("expires 06:12 MDT"), "{text}");
    // Before the expiry nothing changes; after it the line says expired and offers a new link.
    app.set_now(parse_utc_unix("2026-10-14T20:10:00Z").unwrap());
    assert!(screen_text(&app, 140, 40).contains("expires 06:12 MDT"));
    app.set_now(parse_utc_unix("2026-10-14T20:20:00Z").unwrap());
    let text = screen_text(&app, 140, 40);
    let row = text.lines().find(|l| l.contains("Link      ")).unwrap();
    assert!(row.contains("expired 06:12 MDT"), "{row}");
    assert!(row.contains("o new link · d download"), "{row}");
    assert_no_url(&text, "an expired card");
}

#[test]
fn the_footer_lists_the_link_keys_once_a_link_exists_and_is_unchanged_before() {
    for (w, h) in SIZES {
        let text = screen_text(&delivered(false, None), w, h);
        let f = footer(&text);
        assert!(f.contains("o open") && f.contains("d download"), "{w}: {f}");
        assert!(f.contains("q quit"), "{w}: {f}");
        let text = screen_text(&delivered(true, None), w, h);
        assert!(
            footer(&text).contains("o open PR"),
            "{w}: {}",
            footer(&text)
        );
        // A run with nothing delivered keeps the old footer word for word.
        assert_eq!(footer(&screen_text(&working(), w, h)), OLD_FOOTER);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------
// The keys

#[test]
fn o_opens_the_pull_request_when_the_run_has_one() {
    let mut app = delivered(true, Some("/stand/in"));
    let effects = press(&mut app, 'o');
    assert_eq!(
        effects,
        vec![Effect::OpenUrl {
            program: "/stand/in".into(),
            url: "https://example.test/pr/7".into(),
            what: "PR link",
        }]
    );
    assert!(
        screen_text(&app, 100, 30).contains("opening the PR link"),
        "{}",
        screen_text(&app, 100, 30)
    );
}

#[test]
fn o_without_a_pull_request_opens_a_fresh_download_link_and_never_the_url_on_the_card() {
    let mut app = delivered(false, Some("/stand/in"));
    let effects = press(&mut app, 'o');
    assert_eq!(
        effects,
        vec![Effect::OpenDownload {
            program: "/stand/in".into(),
            run_id: RUN.into(),
            artifact_id: ARTIFACT.into(),
        }],
        "the link is made when the key is pressed, so a lapsed one is never opened"
    );
    // The default program is the platform's opener.
    let mut app = delivered(false, None);
    let want = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let effects = press(&mut app, 'o');
    let [Effect::OpenDownload { program, .. }] = effects.as_slice() else {
        panic!("no OpenDownload")
    };
    assert_eq!(program, want);
    // An empty HAMH_OPEN_COMMAND is unset.
    let mut app = delivered(false, Some(""));
    let effects = press(&mut app, 'o');
    let [Effect::OpenDownload { program, .. }] = effects.as_slice() else {
        panic!("no OpenDownload")
    };
    assert_eq!(program, want);
}

#[test]
fn the_answers_to_o_reach_the_notice_line_and_the_fresh_expiry_reaches_the_card() {
    let mut app = delivered(false, Some("/stand/in"));
    app.update(Msg::Opened(Ok(Opened {
        what: "download link",
        expires_at: Some("2026-10-14T21:00:00Z".into()),
    })));
    let text = screen_text(&app, 140, 40);
    assert!(
        text.contains("opened the download link in your browser"),
        "{text}"
    );
    assert!(
        text.contains("expires 21:00 UTC"),
        "the fresh link's expiry:\n{text}"
    );
    app.update(Msg::Opened(Ok(Opened {
        what: "PR link",
        expires_at: None,
    })));
    let text = screen_text(&app, 140, 40);
    assert!(
        text.contains("opened the PR link in your browser"),
        "{text}"
    );
    assert!(text.contains("expires 21:00 UTC"), "{text}");
    app.update(Msg::Opened(Err(
        "could not run open: No such file or directory (set HAMH_OPEN_COMMAND to a program that opens a URL)".into(),
    )));
    let text = screen_text(&app, 140, 40);
    assert!(
        text.contains("could not open the link: could not run open"),
        "{text}"
    );
    assert_no_url(&text, "the notice line");
}

#[test]
fn d_downloads_the_zip_again_through_the_pull_path_and_keeps_the_card() {
    let mut app = delivered(false, None);
    let effects = press(&mut app, 'd');
    assert_eq!(
        effects,
        vec![Effect::Download {
            run_id: RUN.into(),
            artifact_id: ARTIFACT.into(),
        }]
    );
    let text = screen_text(&app, 100, 30);
    assert!(text.contains("Zip       downloading"), "{text}");
    assert!(
        text.contains("Outcome   Cline reported done"),
        "the card stays:\n{text}"
    );
    assert!(
        press(&mut app, 'd').is_empty(),
        "one download at a time, also when the key is pressed again"
    );
    assert!(
        screen_text(&app, 100, 30).contains("already downloading"),
        "{}",
        screen_text(&app, 100, 30)
    );
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(pulled_at(
            "/other/python-calc-R1-abc1234.zip",
            "2026-10-14T21:05:00Z",
        )),
    });
    let text = screen_text(&app, 140, 40);
    assert!(
        text.contains("Zip       /other/python-calc-R1-abc1234.zip"),
        "{text}"
    );
    assert!(
        text.contains("saved /other/python-calc-R1-abc1234.zip (sha256 matched)"),
        "{text}"
    );
    assert!(
        text.contains("expires 21:05 UTC"),
        "the new link's expiry:\n{text}"
    );
    assert_no_url(&text, "the card after d");
}

#[test]
fn a_failed_download_after_d_keeps_the_earlier_zip_on_the_card_and_says_why() {
    let mut app = delivered(false, None);
    press(&mut app, 'd');
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Err("download: sha256 mismatch".into()),
    });
    let text = screen_text(&app, 140, 40);
    assert!(
        text.contains("Zip       /downloads/python-calc-R1-abc1234.zip"),
        "{text}"
    );
    assert!(
        text.contains("download failed: download: sha256 mismatch"),
        "{text}"
    );
    // A first download that failed has no card; d tries again.
    let mut app = app_with(None);
    app.open_run(RUN);
    app.update(pages(events(false, true)));
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Err("GET b/path: HTTP 403".into()),
    });
    let text = screen_text(&app, 100, 30);
    assert!(
        text.contains("download failed: GET b/path: HTTP 403"),
        "{text}"
    );
    assert!(text.contains("d try again"), "{text}");
    assert_eq!(
        press(&mut app, 'd'),
        vec![Effect::Download {
            run_id: RUN.into(),
            artifact_id: ARTIFACT.into(),
        }]
    );
    assert!(screen_text(&app, 100, 30).contains("downloading the zip"));
}

#[test]
fn the_keys_do_nothing_when_there_is_no_link() {
    let mut app = working();
    assert!(press(&mut app, 'o').is_empty());
    assert!(
        screen_text(&app, 100, 30).contains("no link yet"),
        "{}",
        screen_text(&app, 100, 30)
    );
    assert!(press(&mut app, 'd').is_empty());
    assert!(
        screen_text(&app, 100, 30).contains("nothing is delivered yet"),
        "{}",
        screen_text(&app, 100, 30)
    );
    // A pull request without a zip: o opens the pull request, d has no zip to fetch.
    let mut app = app_with(Some("/stand/in"));
    app.open_run(RUN);
    app.update(pages(events(true, false)));
    assert!(press(&mut app, 'd').is_empty());
    let text = screen_text(&app, 140, 40);
    assert!(!footer(&text).contains("d download"), "{}", footer(&text));
    assert!(footer(&text).contains("o open PR"), "{}", footer(&text));
    assert!(matches!(
        press(&mut app, 'o').as_slice(),
        [Effect::OpenUrl {
            what: "PR link",
            ..
        }]
    ));
    // Before a run is open the keys are not the dashboard's.
    let mut fresh = app_with(None);
    assert!(press(&mut fresh, 'o').is_empty());
    assert!(press(&mut fresh, 'd').is_empty());
}

#[test]
fn the_keys_can_be_used_while_the_zip_is_still_downloading() {
    let mut app = app_with(Some("/stand/in"));
    app.open_run(RUN);
    app.update(pages(events(false, true)));
    // The download is on its way; the link is made by the key, so o already works, and d waits for the first download.
    assert!(matches!(
        press(&mut app, 'o').as_slice(),
        [Effect::OpenDownload { .. }]
    ));
    assert!(press(&mut app, 'd').is_empty());
}

// ---------------------------------------------------------------------------------------------------------------------------
// The effects, against the local API and a stand-in for the browser

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

/// A program that records each argument it gets on a line of `opened.txt` and exits with `code`.
fn stand_in(dir: &Path, code: i32) -> (String, PathBuf) {
    let record = dir.join("opened.txt");
    let script = dir.join(format!("opener-{code}.sh"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\nexit {code}\n",
            record.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script.display().to_string(), record)
}

#[tokio::test]
async fn the_opener_gets_the_url_as_one_argument_and_no_shell_reads_it() {
    let fake = Fake::start().await;
    let dir = support::scratch("links-opener");
    let (program, record) = stand_in(&dir, 0);
    // The URL carries shell metacharacters (`&`, `;` and a variable reference) that a shell would split or expand. They
    // are ordinary characters: a test never puts anything on a command line that a host sensor could read as hostile. It
    // has no whitespace: `is_web_link` refuses a link with whitespace, so a URL with a space never reaches the opener.
    let url = "https://example.test/pr/7?a=1&b=2;second;$HOME;end".to_string();
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::OpenUrl {
            program,
            url: url.clone(),
            what: "PR link",
        },
    )
    .await;
    let Some(Msg::Opened(Ok(o))) = msg else {
        panic!("{msg:?}")
    };
    assert_eq!((o.what, o.expires_at), ("PR link", None));
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("{url}\n"),
        "the opener must get the URL as exactly one literal argument: a shell would split it and expand the variable"
    );
}

#[tokio::test]
async fn an_opener_that_is_missing_or_fails_is_reported_without_the_url() {
    let fake = Fake::start().await;
    let dir = support::scratch("links-opener-fail");
    let url = "https://example.test/secret-path?X-Amz-Signature=sigsigsig";
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::OpenUrl {
            program: "/nonexistent/ham-opener".into(),
            url: url.into(),
            what: "PR link",
        },
    )
    .await;
    let Some(Msg::Opened(Err(e))) = msg else {
        panic!("{msg:?}")
    };
    assert!(e.contains("/nonexistent/ham-opener"), "{e}");
    assert!(e.contains("HAMH_OPEN_COMMAND"), "{e}");
    assert!(!e.contains("example.test") && !e.contains("sigsig"), "{e}");
    let (program, _) = stand_in(&dir, 3);
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::OpenUrl {
            program,
            url: url.into(),
            what: "PR link",
        },
    )
    .await;
    let Some(Msg::Opened(Err(e))) = msg else {
        panic!("{msg:?}")
    };
    assert!(e.contains("exit") && e.contains('3'), "{e}");
    assert!(!e.contains("example.test") && !e.contains("sigsig"), "{e}");
}

async fn delivered_run(label: &str) -> (Arc<Fake>, String, String) {
    let fake = Fake::start().await;
    let run = home::new_run(&fake, label).await;
    fake.mem.inner.lock().unwrap().no_pr = true;
    let (artifact, _sha) = fake.mem.complete_run(&run, true).unwrap();
    (fake, run, artifact)
}

fn mints(fake: &Fake, run: &str, artifact: &str) -> usize {
    fake.mem
        .posts(&format!("/v1/runs/{run}/artifacts/{artifact}/download-url"))
        .len()
}

#[tokio::test]
async fn o_makes_a_fresh_download_link_when_pressed_and_hands_it_to_the_opener() {
    let (fake, run, artifact) = delivered_run("links-o").await;
    let dir = support::scratch("links-o");
    let (program, record) = stand_in(&dir, 0);
    assert_eq!(mints(&fake, &run, &artifact), 0);
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::OpenDownload {
            program,
            run_id: run.clone(),
            artifact_id: artifact.clone(),
        },
    )
    .await;
    assert!(
        !format!("{msg:?}").contains("presign-token"),
        "the message carries no URL: {msg:?}"
    );
    let Some(Msg::Opened(Ok(o))) = msg else {
        panic!("{msg:?}")
    };
    assert_eq!(o.what, "download link");
    assert!(
        o.expires_at
            .as_deref()
            .and_then(parse_utc_unix)
            .is_some_and(|t| t > 0),
        "{o:?}"
    );
    assert_eq!(mints(&fake, &run, &artifact), 1, "one link made by the key");
    let got = std::fs::read_to_string(&record).unwrap();
    let args: Vec<&str> = got.lines().collect();
    assert_eq!(args.len(), 1, "{got}");
    assert!(
        args[0].starts_with("http://127.0.0.1:")
            && args[0].contains("/objects/")
            && args[0].contains("X-Amz-Signature=fake"),
        "{got}"
    );
    // Pressed again, it makes another link: a link opened later is never an old one.
    let (program, _) = stand_in(&dir, 0);
    perform(
        &api(&fake),
        &dir,
        Effect::OpenDownload {
            program,
            run_id: run.clone(),
            artifact_id: artifact.clone(),
        },
    )
    .await;
    assert_eq!(mints(&fake, &run, &artifact), 2);
}

#[tokio::test]
async fn d_downloads_with_the_pull_checks_and_replaces_a_lapsed_link() {
    let (fake, run, artifact) = delivered_run("links-d").await;
    let dir = support::scratch("links-d");
    // The first link the download gets has lapsed (the store answers 403): the download makes a second link and goes on.
    fake.faults
        .object_403
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::Download {
            run_id: run.clone(),
            artifact_id: artifact.clone(),
        },
    )
    .await;
    let Some(Msg::Downloaded { result: Ok(p), .. }) = msg else {
        panic!("{msg:?}")
    };
    assert_eq!(
        mints(&fake, &run, &artifact),
        2,
        "the lapsed link was replaced"
    );
    assert!(p.path.starts_with(&dir), "{:?}", p.path);
    let bytes = std::fs::read(&p.path).unwrap();
    assert_eq!(
        support::sha256_hex(&bytes),
        p.artifact.sha256,
        "checksum matched"
    );
    // d again with the file in place re-checks it and fetches nothing.
    let msg = perform(
        &api(&fake),
        &dir,
        Effect::Download {
            run_id: run.clone(),
            artifact_id: artifact.clone(),
        },
    )
    .await;
    let Some(Msg::Downloaded {
        result: Ok(again), ..
    }) = msg
    else {
        panic!("{msg:?}")
    };
    assert!(again.ranges.is_empty(), "nothing was fetched twice");
}

// ---------------------------------------------------------------------------------------------------------------------------
// The whole path: the real event loop, the local API and a stand-in browser

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_and_d_work_through_the_real_event_loop() {
    let (fake, run, artifact) = delivered_run("links-loop").await;
    let dir = support::scratch("links-loop");
    let (program, record) = stand_in(&dir, 0);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    let mut app = app_with(Some(&program));
    let first = app.open_run(&run);
    let driver = tokio::spawn({
        let (fake, dir, run, artifact) = (fake.clone(), dir.clone(), run.clone(), artifact.clone());
        let record = record.clone();
        async move {
            let wait_for = |what: &'static str, mut cond: Box<dyn FnMut() -> bool + Send>| async move {
                for _ in 0..600 {
                    if cond() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                panic!("timed out waiting for {what}");
            };
            let key =
                |c: KeyCode| crossterm::event::Event::Key(KeyEvent::new(c, KeyModifiers::NONE));
            // The zip downloads once on delivery; wait for it, then press o.
            let d2 = dir.clone();
            wait_for(
                "the zip",
                Box::new(move || {
                    std::fs::read_dir(&d2)
                        .unwrap()
                        .filter_map(Result::ok)
                        .any(|e| e.file_name().to_string_lossy().ends_with(".zip"))
                }),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            tx.send(key(KeyCode::Char('o'))).unwrap();
            let r2 = record.clone();
            wait_for("the opener", Box::new(move || r2.exists())).await;
            tx.send(key(KeyCode::Char('d'))).unwrap();
            let (f3, r3, a3) = (fake.clone(), run.clone(), artifact.clone());
            wait_for("the link of d", Box::new(move || mints(&f3, &r3, &a3) >= 3)).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx.send(crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
            tx
        }
    });
    tokio::time::timeout(
        Duration::from_secs(20),
        ham::tui::event_loop(
            &mut term,
            api(&fake),
            dir.clone(),
            app,
            first,
            rx,
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("the event loop did not end")
    .unwrap();
    let _tx = driver.await.unwrap();
    let screen = buffer_text(term.backend(), 140, 40);
    let opened = std::fs::read_to_string(&record).unwrap();
    assert!(opened.starts_with("http://127.0.0.1:"), "{opened}");
    assert!(
        screen.contains("Link      s3 · ") || screen.contains("Link      127.0.0.1 · "),
        "{screen}"
    );
    // The notice line is cut at the terminal width, so only its start is certain.
    assert!(
        screen.lines().any(|l| l.starts_with("saved ")),
        "d reported the saved file:\n{screen}"
    );
    assert_no_url(&screen, "the screen");
    assert!(
        !screen.contains("X-Amz") && !screen.contains("presign-token"),
        "{screen}"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------
// The local clock

#[test]
fn the_local_clock_writes_hours_minutes_and_the_zone_abbreviation() {
    let epoch = parse_utc_unix("2026-10-16T12:12:00Z").unwrap();
    assert_eq!(
        ham::link::local_clock_with("date", epoch, Some("America/Denver")),
        "06:12 MDT"
    );
    assert_eq!(
        ham::link::local_clock_with("date", epoch, Some("UTC")),
        "12:12 UTC"
    );
    // After the clocks go back (2026-11-01) the same zone says MST.
    let winter = parse_utc_unix("2026-11-20T12:12:00Z").unwrap();
    assert_eq!(
        ham::link::local_clock_with("date", winter, Some("America/Denver")),
        "05:12 MST"
    );
    // No date program: the UTC text, so the card still says when.
    assert_eq!(
        ham::link::local_clock_with("/nonexistent/date", epoch, Some("America/Denver")),
        "12:12 UTC"
    );
}
