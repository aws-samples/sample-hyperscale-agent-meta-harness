//! The Runs screen when the stage address does not resolve: the note under the table wraps onto as many rows as the words need (up to four), so the
//! words that say the stage may have been removed or replaced and name `./setup.sh --status` and `ham config set api_endpoint <url>` show on an
//! 80-column terminal as well as on a wide one. A short error keeps its one row, and the key line stays the last row.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::http::TransportError;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Msg};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::Arc;
use std::time::Duration;
use support::{REGION, creds};

const HOST: &str = "bd2ygxd10m.execute-api.us-east-1.amazonaws.com";

fn fresh() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

fn rows(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The rows of the note: from the one that starts `cannot list runs:` to the row above the key line (the last row).
fn note(rows: &[String]) -> Vec<String> {
    let first = rows
        .iter()
        .position(|r| r.starts_with("cannot list runs:"))
        .unwrap_or_else(|| panic!("no note in: {rows:#?}"));
    rows[first..rows.len() - 1].to_vec()
}

fn unresolved_app() -> App {
    let mut app = fresh();
    let e = ApiError::Transport(TransportError::Unresolved(HOST.into()));
    app.update(Msg::RunsRead(Err(e.to_string())));
    app
}

#[test]
fn the_hint_shows_whole_on_eighty_columns() {
    let app = unresolved_app();
    let screen = rows(&app, 80, 24);
    let note = note(&screen);
    assert!(
        (2..=4).contains(&note.len()),
        "the words need two to four rows: {note:#?}"
    );
    assert!(
        note[0].starts_with("cannot list runs: the stage address does not resolve"),
        "{note:#?}"
    );
    let joined = note.join(" ");
    for want in [
        HOST,
        "may have been removed or replaced",
        "`./setup.sh --status`",
        "`ham config set api_endpoint <url>`",
    ] {
        assert!(joined.contains(want), "{want:?} missing from: {note:#?}");
    }
    assert!(
        screen.last().unwrap().contains("q quit"),
        "the key line is the last row: {screen:#?}"
    );
    for row in &screen {
        assert!(row.chars().count() <= 80, "{row:?}");
    }
}

#[test]
fn the_hint_shows_whole_at_every_common_width() {
    let app = unresolved_app();
    for (w, h) in [(80, 24), (100, 30), (120, 40), (200, 50)] {
        let screen = rows(&app, w, h);
        let note = note(&screen);
        assert!(note.len() <= 4, "{w}x{h}: {note:#?}");
        let joined = note.join(" ");
        for want in [
            "may have been removed or replaced",
            "`./setup.sh --status`",
            "`ham config set api_endpoint <url>`",
        ] {
            assert!(joined.contains(want), "{w}x{h}: {want:?} in {note:#?}");
        }
        assert!(!joined.contains('…'), "{w}x{h}: nothing is cut: {note:#?}");
        assert!(
            screen.last().unwrap().contains("q quit"),
            "{w}x{h}: {screen:#?}"
        );
    }
}

#[test]
fn a_narrow_terminal_still_leads_with_the_words_and_keeps_the_key_line_last() {
    // 60 columns cut the second command at the fourth row, as the key line is cut at its edge; the first sentences and the first command show.
    let app = unresolved_app();
    let screen = rows(&app, 60, 24);
    let note = note(&screen);
    assert_eq!(note.len(), 4, "{note:#?}");
    let joined = note.join(" ");
    for want in ["may have been removed or replaced", "`./setup.sh --status`"] {
        assert!(joined.contains(want), "{want:?} in {note:#?}");
    }
    assert!(
        screen.last().unwrap().starts_with("n new run"),
        "{screen:#?}"
    );
}

#[test]
fn a_short_error_keeps_its_one_row_above_the_key_line() {
    let mut app = fresh();
    app.update(Msg::RunsRead(Err("HTTP 403: not authorized (http)".into())));
    let screen = rows(&app, 80, 24);
    let n = screen.len();
    assert_eq!(
        screen[n - 2],
        "cannot list runs: HTTP 403: not authorized (http)",
        "{screen:#?}"
    );
    assert!(screen[n - 1].contains("q quit"), "{screen:#?}");
}

#[test]
fn another_long_error_wraps_too_and_carries_no_hint() {
    let long = format!(
        "network: GET https://{HOST}/direct/v1/runs: client error (Connect): tcp connect error: Connection refused (os error 111) and then some more words to pass the edge"
    );
    let mut app = fresh();
    app.update(Msg::RunsRead(Err(long)));
    let screen = rows(&app, 80, 24);
    let note = note(&screen);
    assert!((2..=4).contains(&note.len()), "{note:#?}");
    let joined = note.join(" ");
    assert!(
        joined.contains(
            "Connection refused (os error 111) and then some more words to pass the edge"
        ),
        "{note:#?}"
    );
    assert!(!joined.contains("removed or replaced"), "{note:#?}");
    assert!(screen.last().unwrap().contains("q quit"), "{screen:#?}");
}

#[test]
fn an_error_longer_than_four_rows_ends_with_an_ellipsis() {
    let very_long = format!("network: {}", "word ".repeat(120));
    let mut app = fresh();
    app.update(Msg::RunsRead(Err(very_long)));
    let screen = rows(&app, 80, 24);
    let note = note(&screen);
    assert_eq!(note.len(), 4, "{note:#?}");
    assert!(note[3].ends_with('…'), "{note:#?}");
    assert!(screen.last().unwrap().contains("q quit"), "{screen:#?}");
}

#[tokio::test]
async fn a_real_lookup_failure_reaches_the_runs_screen_with_the_hint() {
    // The whole path of the compiled app: the transport fails to resolve a name under `.invalid`, the read maps the error to the text the
    // screen gets (`e.to_string()`, as `tui::run` does), and the Runs screen shows the words.
    let plane = HttpsPlane::new(
        "https://ham-stage-gone.invalid/direct",
        Signer::new(REGION, creds()),
    )
    .unwrap();
    let api = Api::new(Arc::new(plane)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let e = api.whoami().await.unwrap_err().to_string();
    let mut app = fresh();
    app.update(Msg::RunsRead(Err(e)));
    let screen = rows(&app, 80, 24);
    let note = note(&screen);
    let joined = note.join(" ");
    for want in [
        "cannot list runs: the stage address does not resolve (ham-stage-gone.invalid).",
        "may have been removed or replaced",
        "`./setup.sh --status`",
        "`ham config set api_endpoint <url>`",
    ] {
        assert!(joined.contains(want), "{want:?} missing from: {note:#?}");
    }
    assert!(!joined.contains("network:"), "{note:#?}");
}
