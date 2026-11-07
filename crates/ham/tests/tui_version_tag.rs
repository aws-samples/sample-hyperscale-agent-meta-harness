//! The version tag. Every screen of the
//! TUI shows `ham vN.NN | backend vN.NN` right-aligned on its top row: the client's version is compiled in from the root `VERSION` file, the
//! backend's is the `x-hamh-backend-version` header of the last `/v1` answer. Until the backend has answered, and for a backend that sends no
//! valid header, the backend half reads `v?.??`. The tag is an overlay on row 0: the titles of the screens stay as they are, leftmost, and are
//! clipped with an ellipsis when they would run into it. The tag shortens on a narrow terminal and is never hidden.
//!
//! This file holds the pure formatter, the style, the overlay on a buffer, the tag on every screen through ratatui's TestBackend, the cell
//! the live transport fills, `ham --version` and the `versions` line of `ham doctor`. The scripted run in a pseudo-terminal is in
//! `crates/ham-e2e`.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::doctor;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::version_tag::{self, GAP, TITLE_KEEP};
use ham::tui::{App, DirectSettings, Msg};
use ham::version::{self, BACKEND_HEADER, CLIENT};
use harness_protocol::api::v1::{ArtifactView, EventsPage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use support::{BackendHeader, Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

/// The tests that pin the process-wide backend cell run one at a time.
static CELL: Mutex<()> = Mutex::new(());

fn pinned(backend: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = CELL.lock().unwrap_or_else(PoisonError::into_inner);
    version::set_backend_for_test(backend);
    guard
}

fn full(backend: &str) -> String {
    format!("ham v{CLIENT} | backend v{backend}")
}

// ---- the version file --------------------------------------------------------------------------------------------------------------

#[test]
fn the_client_version_is_the_root_version_file() {
    let file =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../VERSION")).unwrap();
    assert_eq!(file.trim(), CLIENT);
    assert!(version::is_valid(CLIENT), "{CLIENT}");
    assert_eq!(BACKEND_HEADER, "x-hamh-backend-version");
}

#[test]
fn only_n_dot_two_digits_is_a_version() {
    for ok in ["4.01", "0.00", "4.99", "10.02", "123.45"] {
        assert!(version::is_valid(ok), "{ok}");
    }
    for bad in [
        "", "4", "4.1", "4.001", ".01", "v4.01", "4.01 ", " 4.01", "4.01\n", "a.bc", "4.0a",
        "4,01", "4.01.02", "-4.01", "?.??", "٤.٠١",
    ] {
        assert!(!version::is_valid(bad), "{bad:?}");
    }
}

// ---- the cell the live transport fills -------------------------------------------------------------------------------------------

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn the_backend_cell_follows_the_last_answer() {
    let _g = pinned(None);
    assert_eq!(version::backend(), None, "nothing answered yet");
    version::note_backend(&headers(&[(BACKEND_HEADER, "4.01")]));
    assert_eq!(version::backend().as_deref(), Some("4.01"));
    version::note_backend(&headers(&[
        ("content-type", "application/json"),
        (BACKEND_HEADER, "4.02"),
    ]));
    assert_eq!(
        version::backend().as_deref(),
        Some("4.02"),
        "the last valid value wins"
    );
    version::note_backend(&headers(&[("content-type", "application/json")]));
    assert_eq!(
        version::backend(),
        None,
        "an answer without the header says the backend does not report one"
    );
    version::note_backend(&headers(&[(BACKEND_HEADER, "4.03")]));
    assert_eq!(version::backend().as_deref(), Some("4.03"));
    for invalid in [
        "",
        "4",
        "4.1",
        "v4.03",
        "4.03 ",
        "4.003",
        "latest",
        "4.03; drop table",
    ] {
        version::note_backend(&headers(&[(BACKEND_HEADER, invalid)]));
        assert_eq!(version::backend(), None, "{invalid:?} is no version");
        version::note_backend(&headers(&[(BACKEND_HEADER, "4.03")]));
    }
    version::set_backend_for_test(Some("9.99"));
    assert_eq!(version::backend().as_deref(), Some("9.99"));
    version::set_backend_for_test(None);
    assert_eq!(version::backend(), None);
}

fn plane(fake: &Fake) -> HttpsPlane {
    HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap()
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(plane(fake))).with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell; nextest runs each test alone
#[tokio::test]
async fn a_signed_answer_fills_the_cell_and_an_unsigned_fetch_does_not() {
    use ham::http::ControlPlane;
    let _g = pinned(None);
    let fake = Fake::start().await;
    let p = plane(&fake);
    // The stand-in answers like the backend: its own version in the header of every answer, the 404 of a missing run included.
    let ok = p.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(ok.status, 200);
    assert_eq!(
        ok.headers.get(BACKEND_HEADER).map(String::as_str),
        Some(CLIENT)
    );
    assert_eq!(version::backend().as_deref(), Some(CLIENT));
    version::set_backend_for_test(None);
    let missing = p
        .call(
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events?shard=7",
            &[],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status, 404);
    assert_eq!(
        version::backend().as_deref(),
        Some(CLIENT),
        "a 404 carries the header too"
    );
    // A presigned object URL is S3's answer, not the backend's: it says nothing about the backend.
    version::set_backend_for_test(Some("3.50"));
    let object = p
        .fetch(
            &format!(
                "{}/objects/missing?X-Amz-Signature=x",
                fake.endpoint.trim_end_matches("/live")
            ),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(object.status >= 400);
    assert_eq!(
        version::backend().as_deref(),
        Some("3.50"),
        "fetch leaves the cell alone"
    );
}

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell; nextest runs each test alone
#[tokio::test]
async fn a_backend_without_the_header_or_with_a_bad_one_reads_unknown() {
    use ham::http::ControlPlane;
    let _g = pinned(Some("4.01"));
    let fake = Fake::start().await;
    let p = plane(&fake);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Absent;
    p.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(version::backend(), None, "an older backend sends no header");
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value("3.99".into());
    p.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(version::backend().as_deref(), Some("3.99"));
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value("not-a-version".into());
    p.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(version::backend(), None, "an invalid value is unknown");
}

// ---- the formatter -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_longest_form_that_fits_is_chosen() {
    let t = |avail| version_tag::tag("4.01", Some("4.01"), avail);
    assert_eq!(t(80).as_deref(), Some("ham v4.01 | backend v4.01"));
    assert_eq!(t(25).as_deref(), Some("ham v4.01 | backend v4.01"));
    assert_eq!(t(24).as_deref(), Some("h v4.01 | b v4.01"));
    assert_eq!(t(17).as_deref(), Some("h v4.01 | b v4.01"));
    assert_eq!(t(16).as_deref(), Some("4.01|4.01"));
    assert_eq!(t(9).as_deref(), Some("4.01|4.01"));
    assert_eq!(t(8), None);
    assert_eq!(t(0), None);
}

#[test]
fn an_unknown_backend_keeps_its_slot() {
    let t = |avail| version_tag::tag("4.01", None, avail);
    assert_eq!(t(80).as_deref(), Some("ham v4.01 | backend v?.??"));
    assert_eq!(t(24).as_deref(), Some("h v4.01 | b v?.??"));
    assert_eq!(t(9).as_deref(), Some("4.01|?.??"));
    assert_eq!(t(8), None);
    // The slot is as wide as a known version's, so the tag does not move when the backend answers.
    for avail in [9, 17, 25] {
        assert_eq!(
            t(avail).unwrap().chars().count(),
            version_tag::tag("4.01", Some("4.01"), avail)
                .unwrap()
                .chars()
                .count()
        );
    }
}

#[test]
fn a_different_backend_is_written_as_it_is() {
    assert_eq!(
        version_tag::tag("4.01", Some("4.00"), 80).as_deref(),
        Some("ham v4.01 | backend v4.00")
    );
    assert_eq!(
        version_tag::tag("4.01", Some("3.99"), 20).as_deref(),
        Some("h v4.01 | b v3.99")
    );
    assert_eq!(
        version_tag::tag("4.01", Some("5.02"), 9).as_deref(),
        Some("4.01|5.02")
    );
}

#[test]
fn a_two_digit_release_number_changes_the_widths_not_the_shape() {
    let t = |avail| version_tag::tag("10.02", Some("10.02"), avail);
    assert_eq!(t(27).as_deref(), Some("ham v10.02 | backend v10.02"));
    assert_eq!(t(26).as_deref(), Some("h v10.02 | b v10.02"));
    assert_eq!(t(19).as_deref(), Some("h v10.02 | b v10.02"));
    assert_eq!(t(18).as_deref(), Some("10.02|10.02"));
    assert_eq!(t(11).as_deref(), Some("10.02|10.02"));
    assert_eq!(t(10), None);
}

#[test]
fn a_tag_never_exceeds_the_room_and_is_never_shorter_than_it_could_be() {
    let versions = [
        ("4.01", Some("4.01")),
        ("4.01", None),
        ("10.02", Some("3.99")),
        ("123.45", Some("123.45")),
    ];
    for (client, backend) in versions {
        let widest = version_tag::tag(client, backend, 200)
            .unwrap()
            .chars()
            .count();
        let smallest = format!("{client}|{}", backend.unwrap_or("?.??"))
            .chars()
            .count();
        for avail in 0..120usize {
            match version_tag::tag(client, backend, avail) {
                None => assert!(
                    avail < smallest,
                    "{client} {backend:?} {avail}: a form fits"
                ),
                Some(t) => {
                    let n = t.chars().count();
                    assert!(n <= avail, "{t:?} in {avail}");
                    if avail >= widest {
                        assert_eq!(n, widest, "{t:?} in {avail}: the full form fits");
                    }
                }
            }
        }
    }
}

#[test]
fn whether_the_backend_differs() {
    assert!(!version_tag::differs("4.01", Some("4.01")));
    assert!(version_tag::differs("4.01", Some("4.00")));
    assert!(version_tag::differs("4.01", Some("4.02")));
    assert!(
        version_tag::differs("4.01", None),
        "unknown is shown as a difference"
    );
}

// ---- the style ---------------------------------------------------------------------------------------------------------------------

fn alert_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

#[test]
fn the_backend_half_is_bold_yellow_when_it_differs_or_is_unknown_and_the_rest_is_dim() {
    for (avail, backend_text) in [(80, "backend v4.00"), (20, "b v4.00"), (9, "4.00")] {
        let tag = version_tag::compose("4.01", Some("4.00"), avail).unwrap();
        assert!(tag.alert());
        let spans = tag.spans();
        assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            tag.text()
        );
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(
            spans[0]
                .content
                .ends_with(if avail == 9 { "|" } else { " | " }),
            "{spans:?}"
        );
        assert_eq!(spans[0].style, Style::default().add_modifier(Modifier::DIM));
        assert_eq!(spans[1].content, backend_text);
        assert_eq!(spans[1].style, alert_style());
    }
    let unknown = version_tag::compose("4.01", None, 80).unwrap();
    assert!(unknown.alert());
    assert_eq!(unknown.spans()[1].content, "backend v?.??");
    assert_eq!(unknown.spans()[1].style, alert_style());
    let same = version_tag::compose("4.01", Some("4.01"), 80).unwrap();
    assert!(!same.alert());
    assert_eq!(
        same.spans()[1].style,
        Style::default().add_modifier(Modifier::DIM),
        "no colour when the versions agree"
    );
    // Bold and dim are style attributes: they show on a terminal with no colour.
    assert!(alert_style().add_modifier.contains(Modifier::BOLD));
}

// ---- reading a tag back ------------------------------------------------------------------------------------------------------------

#[test]
fn split_tag_takes_the_exact_shapes_off_the_end_of_a_row() {
    let cases = [
        (
            "ham · Runs                       ham v4.01 | backend v4.01",
            "ham · Runs",
            Some("ham v4.01 | backend v4.01"),
        ),
        (
            "ham · Runs     h v4.01 | b v4.01",
            "ham · Runs",
            Some("h v4.01 | b v4.01"),
        ),
        ("ham · Runs     4.01|4.01", "ham · Runs", Some("4.01|4.01")),
        (
            "ham · Runs  ham v4.01 | backend v?.??",
            "ham · Runs",
            Some("ham v4.01 | backend v?.??"),
        ),
        (
            "ham · Runs  h v10.02 | b v3.99",
            "ham · Runs",
            Some("h v10.02 | b v3.99"),
        ),
        (
            "ham · Run 01K6A0000000000000000000R1 · Claude Sonnet 5.5 (b…  h v4.01 | b v4.01",
            "ham · Run 01K6A0000000000000000000R1 · Claude Sonnet 5.5 (b…",
            Some("h v4.01 | b v4.01"),
        ),
        (
            "                    ham v4.01 | backend v4.01",
            "",
            Some("ham v4.01 | backend v4.01"),
        ),
        (
            "ham v4.01 | backend v4.01",
            "",
            Some("ham v4.01 | backend v4.01"),
        ),
        ("4.01|?.??", "", Some("4.01|?.??")),
    ];
    for (row, title, tag) in cases {
        assert_eq!(version_tag::split_tag(row), (title, tag), "{row:?}");
    }
}

#[test]
fn split_tag_leaves_a_row_that_only_looks_like_a_tag() {
    for row in [
        "ham · Runs",
        "",
        "ham v4.01 | backend v4.01 and more",
        "ham v4.1 | backend v4.01",
        "ham v4.01 | backend v4.0",
        "ham v4.01 | backend 4.01",
        "ham v4.01 | b v4.01",
        "h v4.01 | backend v4.01",
        "ham · Runs 4.01 | 4.01",
        "ham · Runs  ham v4.01 |backend v4.01",
        "ham · Runs  ham v?.?? | backend v4.01",
        "ham · Runs 4.01|4.01x",
    ] {
        assert_eq!(version_tag::split_tag(row), (row, None), "{row:?}");
    }
}

// ---- the overlay on a buffer -------------------------------------------------------------------------------------------------------

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

/// The overlay over the whole buffer.
fn over(buf: &mut Buffer, client: &str, backend: Option<&str>) {
    let area = buf.area;
    version_tag::overlay(buf, area, client, backend);
}

fn title_buffer(width: u16, title: &str) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, 3));
    buf.set_string(0, 0, title, Style::default().add_modifier(Modifier::BOLD));
    buf.set_string(0, 1, "second row", Style::default());
    buf
}

#[test]
fn the_overlay_writes_the_tag_at_the_right_edge_and_leaves_the_title() {
    let mut buf = title_buffer(80, "ham · Runs");
    over(&mut buf, "4.01", Some("4.01"));
    let row = row_text(&buf, 0);
    assert_eq!(row.chars().count(), 80);
    assert!(row.starts_with("ham · Runs "), "{row:?}");
    assert!(row.ends_with("ham v4.01 | backend v4.01"), "{row:?}");
    assert_eq!(row.trim_end(), row, "the tag ends in the last column");
    assert_eq!(
        row_text(&buf, 1).trim_end(),
        "second row",
        "only row 0 is touched"
    );
    // The tag cells carry the tag's styles, not the bold of the title they replaced.
    let first = &buf[(80 - 25, 0)];
    assert_eq!(first.symbol(), "h");
    assert!(!first.modifier.contains(Modifier::BOLD), "{first:?}");
    assert!(first.modifier.contains(Modifier::DIM), "{first:?}");
    // The title cells keep theirs.
    assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_title_that_would_touch_the_tag_is_clipped_with_an_ellipsis_one_cell_before_the_gap() {
    // 40 columns: the tag is the 9-column form at columns 31..=39, the gap is column 30 and the title may use columns 0..=29.
    let title = "ham · New run · model: Claude Sonnet 5.5 (built-in default)";
    let mut buf = title_buffer(40, title);
    over(&mut buf, "4.01", Some("4.01"));
    let row = row_text(&buf, 0);
    let expected_title: String = title.chars().take(29).collect();
    assert_eq!(row, format!("{expected_title}… 4.01|4.01"), "{row:?}");
    assert_eq!(buf[(29, 0)].symbol(), "…");
    assert_eq!(buf[(30, 0)].symbol(), " ");
    assert_eq!(buf[(31, 0)].symbol(), "4");
    assert_eq!(GAP, 1);
}

#[test]
fn a_title_that_ends_before_the_gap_is_not_touched() {
    // Exactly the room: 40 columns leave columns 0..=29 for the title.
    let title: String = "x".repeat(30);
    let mut buf = title_buffer(40, &title);
    over(&mut buf, "4.01", Some("4.01"));
    assert_eq!(row_text(&buf, 0), format!("{title} 4.01|4.01"));
    let title: String = "x".repeat(31);
    let mut buf = title_buffer(40, &title);
    over(&mut buf, "4.01", Some("4.01"));
    assert_eq!(row_text(&buf, 0), format!("{}… 4.01|4.01", "x".repeat(29)));
}

#[test]
fn a_wide_character_across_the_cut_is_removed_whole() {
    let title = format!("ab{}", "日".repeat(30));
    let mut buf = title_buffer(40, &title);
    over(&mut buf, "4.01", Some("4.01"));
    assert_eq!(buf[(29, 0)].symbol(), "…");
    assert_eq!(
        buf[(28, 0)].symbol(),
        " ",
        "the wide character that reached column 29 is gone"
    );
    assert_eq!(buf[(26, 0)].symbol(), "日");
    assert_eq!(buf[(31, 0)].symbol(), "4");
}

#[test]
fn a_border_on_the_top_row_is_replaced_under_the_tag_without_an_ellipsis() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
    buf.set_string(
        0,
        0,
        format!("┌ Open anything · Enter opens {}┐", "─".repeat(80 - 29 - 1)),
        Style::default(),
    );
    over(&mut buf, "4.01", Some("4.01"));
    let row = row_text(&buf, 0);
    assert!(
        row.starts_with("┌ Open anything · Enter opens ─"),
        "{row:?}"
    );
    assert!(row.ends_with("ham v4.01 | backend v4.01"), "{row:?}");
    assert!(!row.contains('…'), "{row:?}");
    assert!(!row.contains('┐'), "{row:?}");
    assert_eq!(buf[(80 - 26, 0)].symbol(), " ", "the gap is blank");
}

#[test]
fn an_empty_top_row_gets_just_the_tag_and_a_tiny_frame_gets_none() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 60, 2));
    over(&mut buf, "4.01", None);
    assert_eq!(row_text(&buf, 0).trim_start(), "h v4.01 | b v?.??");
    let mut small = Buffer::empty(Rect::new(0, 0, 8, 2));
    over(&mut small, "4.01", Some("4.01"));
    assert_eq!(row_text(&small, 0).trim(), "");
    let mut none = Buffer::empty(Rect::new(0, 0, 40, 0));
    over(&mut none, "4.01", Some("4.01"));
}

#[test]
fn the_overlay_respects_the_area_it_is_given() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 100, 5));
    version_tag::overlay(&mut buf, Rect::new(10, 2, 70, 3), "4.01", Some("4.01"));
    let row = row_text(&buf, 2);
    assert_eq!(&row[..10], " ".repeat(10), "{row:?}");
    assert!(
        row[10..80].ends_with("ham v4.01 | backend v4.01"),
        "{row:?}"
    );
    assert_eq!(row[80..].trim(), "", "nothing right of the area");
    assert_eq!(row_text(&buf, 0).trim(), "");
}

// ---- the widths --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_form_follows_the_width_of_the_terminal() {
    // The title keeps TITLE_KEEP columns (`ham · Run ` and a whole run id), then the gap, then the tag.
    assert_eq!(TITLE_KEEP, 36);
    let form =
        |w: usize| version_tag::for_frame("4.01", Some("4.01"), w).map(|t| t.text().to_string());
    assert_eq!(form(200).as_deref(), Some("ham v4.01 | backend v4.01"));
    assert_eq!(form(80).as_deref(), Some("ham v4.01 | backend v4.01"));
    assert_eq!(form(62).as_deref(), Some("ham v4.01 | backend v4.01"));
    assert_eq!(form(61).as_deref(), Some("h v4.01 | b v4.01"));
    assert_eq!(form(60).as_deref(), Some("h v4.01 | b v4.01"));
    assert_eq!(form(54).as_deref(), Some("h v4.01 | b v4.01"));
    assert_eq!(form(53).as_deref(), Some("4.01|4.01"));
    assert_eq!(form(40).as_deref(), Some("4.01|4.01"));
    assert_eq!(form(30).as_deref(), Some("4.01|4.01"));
    assert_eq!(form(20).as_deref(), Some("4.01|4.01"));
    assert_eq!(form(9).as_deref(), Some("4.01|4.01"));
    assert_eq!(form(8), None);
    // A narrower terminal never gets a longer form.
    let mut last = usize::MAX;
    for w in (0..200).rev() {
        let n = form(w).map_or(0, |t| t.chars().count());
        assert!(n <= last, "{w}");
        last = n;
    }
}

#[test]
fn reserve_is_the_width_of_the_tag_and_its_gap() {
    let _g = pinned(Some(CLIENT));
    let tag_width = |w: u16| version_tag::reserve(w);
    let n = CLIENT.chars().count();
    assert_eq!(
        tag_width(80),
        format!("ham v{CLIENT} | backend v{CLIENT}").chars().count() + GAP
    );
    assert_eq!(
        tag_width(60),
        format!("h v{CLIENT} | b v{CLIENT}").chars().count() + GAP
    );
    assert_eq!(tag_width(40), 2 * n + 1 + GAP);
    assert_eq!(tag_width(8), 0, "no tag, nothing reserved");
}

// ---- every screen ------------------------------------------------------------------------------------------------------------------

fn new_app() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn harness(n: u32, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-10-15T13:41:07Z".into(),
        event_type: t,
        authority: Authority::Harness,
        payload: Payload::Inline { value: v },
    }
}

fn question() -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: REQ.into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 0,
        ts: "2026-10-15T13:41:07Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({
                "turn": 1, "kind": "needs-input", "text": "Needs your input: Cline stopped its own prompt",
                "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "the engine ended the prompt cancelled", "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling the build.", "files": ["PROGRESS.md"], "filesTotal": 1,
                "blocking": true, "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [{"id": "us.anthropic.claude-opus-5-5", "name": "Claude Opus 5.5", "contextTokens": 1000000}],
                "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-15T14:11:07Z"
            }),
        },
    }
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

fn working() -> Vec<Envelope> {
    vec![
        harness(
            1,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        harness(
            2,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": ham_core::models::DEFAULT_MODEL_ID}),
        ),
        harness(
            3,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

fn dashboard() -> App {
    let mut app = new_app();
    app.open_run(RUN);
    app.update(pages(working()));
    app
}

fn stuck() -> App {
    let mut events = working();
    events.push(question());
    let mut app = new_app();
    app.open_run(RUN);
    app.update(pages(events));
    app
}

/// Every screen of the TUI, by the part of its title that is whole at 80 columns and wider.
fn screens() -> Vec<(&'static str, App, String)> {
    let mut out: Vec<(&'static str, App, String)> = Vec::new();
    out.push(("runs", new_app(), "ham · Runs".into()));

    let mut composer = new_app();
    composer.handle_key(key(KeyCode::Char('n')));
    out.push(("new run", composer, "ham · New run · model: ".into()));

    out.push(("run", dashboard(), format!("ham · Run {RUN}")));

    let mut typing = dashboard();
    for c in "/help".chars() {
        typing.handle_key(key(KeyCode::Char(c)));
    }
    typing.handle_key(key(KeyCode::Enter));
    out.push((
        "run with the help panel",
        typing,
        format!("ham · Run {RUN}"),
    ));

    let mut reply = stuck();
    reply.handle_key(key(KeyCode::Char('p')));
    // At 80 columns the end of these two titles is clipped; the part given here is whole at every width the tests use.
    out.push(("reply", reply, format!("ham · Run {RUN} · message")));

    let mut models = stuck();
    models.handle_key(key(KeyCode::Char('m')));
    out.push(("models", models, format!("ham · Run {RUN} · choose")));

    let mut artifacts = dashboard();
    artifacts.handle_key(key(KeyCode::Char('a')));
    artifacts.update(Msg::Artifacts(Ok(vec![ArtifactView {
        artifact_id: "01K6A00000000000000000ZP01".into(),
        kind: ArtifactKind::Zip,
        s3_uri: format!("s3://b/runs/{RUN}/deliverable/x.zip"),
        sha256: "ab".repeat(32),
        size_bytes: 2048,
    }])));
    out.push(("artifacts", artifacts, format!("ham · Artifacts {RUN}")));

    // The Settings screen: `s` on the Runs list, shown while its file is being read.
    let mut settings = new_app();
    settings.handle_key(key(KeyCode::Char('s')));
    out.push(("settings", settings, "ham · Settings".into()));

    let mut open = new_app();
    open.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    out.push(("open anything", open, "┌ Open anything".into()));
    out
}

fn draw(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn tag_of(row: &str, w: u16, h: u16, name: &str) -> String {
    let (_, tag) = version_tag::split_tag(row.trim_end());
    tag.unwrap_or_else(|| panic!("{name} at {w}x{h} has no tag on its top row: {row:?}"))
        .to_string()
}

#[test]
fn every_screen_shows_the_tag_right_aligned_on_its_top_row() {
    let _g = pinned(Some(CLIENT));
    for (name, app, title) in screens() {
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (200, 60)] {
            let buf = draw(&app, w, h);
            let row = row_text(&buf, 0);
            assert_eq!(row.chars().count(), usize::from(w), "{name}");
            assert_eq!(
                tag_of(&row, w, h, name),
                full(CLIENT),
                "{name} at {w}x{h}: {row:?}"
            );
            assert!(row.ends_with(&full(CLIENT)), "{name} at {w}x{h}: {row:?}");
            assert!(
                row.starts_with(&title),
                "{name} at {w}x{h}: the title stays leftmost and whole: {row:?}"
            );
            // At least the gap between the title and the tag.
            let (left, _) = version_tag::split_tag(row.trim_end());
            assert!(
                left.chars().count() + GAP + full(CLIENT).chars().count() <= usize::from(w),
                "{name} {row:?}"
            );
        }
    }
}

#[test]
fn the_titles_are_the_titles_they_were() {
    let _g = pinned(Some(CLIENT));
    let want: BTreeMap<&str, String> = BTreeMap::from([
        ("runs", "ham · Runs".to_string()),
        (
            "run",
            format!("ham · Run {RUN} · Claude Sonnet 5.5 · active"),
        ),
        ("reply", format!("ham · Run {RUN} · message to Cline")),
        ("models", format!("ham · Run {RUN} · choose another model")),
        ("artifacts", format!("ham · Artifacts {RUN}")),
    ]);
    for (name, app, _) in screens() {
        let Some(title) = want.get(name) else {
            continue;
        };
        let row = row_text(&draw(&app, 100, 30), 0);
        let (left, _) = version_tag::split_tag(row.trim_end());
        assert!(
            left.starts_with(title.as_str()),
            "{name}: {left:?} does not start with {title:?}"
        );
    }
    let (_, composer, _) = screens()
        .into_iter()
        .find(|(n, ..)| *n == "new run")
        .unwrap();
    let row = row_text(&draw(&composer, 120, 40), 0);
    assert!(
        row.starts_with("ham · New run · model: Claude Sonnet 5.5 (built-in default)"),
        "{row:?}"
    );
}

#[test]
fn the_tag_is_there_at_every_width_in_the_form_that_fits() {
    let _g = pinned(Some(CLIENT));
    let forms = [
        (80u16, format!("ham v{CLIENT} | backend v{CLIENT}")),
        (62, format!("ham v{CLIENT} | backend v{CLIENT}")),
        (60, format!("h v{CLIENT} | b v{CLIENT}")),
        (54, format!("h v{CLIENT} | b v{CLIENT}")),
        (40, format!("{CLIENT}|{CLIENT}")),
        (30, format!("{CLIENT}|{CLIENT}")),
        (20, format!("{CLIENT}|{CLIENT}")),
    ];
    for (name, app, _) in screens() {
        for (w, tag) in &forms {
            let buf = draw(&app, *w, 24);
            let row = row_text(&buf, 0);
            assert!(
                row.ends_with(tag.as_str()),
                "{name} at {w}: {row:?} should end with {tag:?}"
            );
            assert_eq!(tag_of(&row, *w, 24, name), *tag, "{name} at {w}");
        }
    }
}

#[test]
fn a_title_that_runs_into_the_tag_is_clipped_and_the_tag_stays_whole() {
    let _g = pinned(Some(CLIENT));
    let (_, composer, _) = screens()
        .into_iter()
        .find(|(n, ..)| *n == "new run")
        .unwrap();
    // 60 columns: the 17-column form at columns 43..=59, the gap at 42, the title in columns 0..=41 with the ellipsis in the last of them.
    let row = row_text(&draw(&composer, 60, 24), 0);
    let title = "ham · New run · model: Claude Sonnet 5.5 (built-in default)";
    let kept: String = title.chars().take(41).collect();
    assert_eq!(row, format!("{kept}… h v{CLIENT} | b v{CLIENT}"), "{row:?}");
    // 40 columns: the 9-column form.
    let row = row_text(&draw(&composer, 40, 24), 0);
    let kept: String = title.chars().take(29).collect();
    assert_eq!(row, format!("{kept}… {CLIENT}|{CLIENT}"), "{row:?}");
}

#[test]
fn the_run_header_gives_up_its_tail_for_the_tag_and_keeps_the_run_id_and_the_state() {
    let _g = pinned(Some(CLIENT));
    let app = dashboard();
    // 80 columns: the header has 80 - 26 = 54 columns. The model is the first part it gives up; the id and the state stay whole.
    let row = row_text(&draw(&app, 80, 24), 0);
    let (left, tag) = version_tag::split_tag(row.trim_end());
    assert_eq!(tag, Some(full(CLIENT).as_str()));
    assert_eq!(left, format!("ham · Run {RUN} · active"), "{row:?}");
    assert!(!left.contains('…'), "{row:?}");
    // 120 columns: everything fits as before.
    let row = row_text(&draw(&app, 120, 40), 0);
    let (left, _) = version_tag::split_tag(row.trim_end());
    assert!(
        left.starts_with(&format!("ham · Run {RUN} · Claude Sonnet 5.5 · active")),
        "{row:?}"
    );
}

#[test]
fn before_the_backend_has_answered_the_tag_says_so() {
    let _g = pinned(None);
    for (name, app, _) in screens() {
        let buf = draw(&app, 100, 30);
        let row = row_text(&buf, 0);
        assert!(
            row.ends_with(&format!("ham v{CLIENT} | backend v?.??")),
            "{name}: {row:?}"
        );
    }
}

#[test]
fn the_tag_changes_when_the_backend_answers() {
    let _g = pinned(None);
    let app = dashboard();
    let before = row_text(&draw(&app, 100, 30), 0);
    version::note_backend(&headers(&[(BACKEND_HEADER, CLIENT)]));
    let after = row_text(&draw(&app, 100, 30), 0);
    assert!(before.ends_with("backend v?.??"));
    assert!(after.ends_with(&format!("backend v{CLIENT}")));
    assert_eq!(
        before.chars().count(),
        after.chars().count(),
        "the tag does not move"
    );
}

fn backend_cells(buf: &Buffer, tag: &str, backend_text: &str) -> Vec<Style> {
    let width = buf.area.width;
    let start = width - u16::try_from(tag.chars().count()).unwrap();
    let from = start + u16::try_from(tag.chars().count() - backend_text.chars().count()).unwrap();
    (from..width).map(|x| buf[(x, 0)].style()).collect()
}

#[test]
fn a_backend_that_differs_or_is_unknown_is_bold_yellow_and_one_that_agrees_is_dim() {
    let app = dashboard();
    {
        let _g = pinned(Some(CLIENT));
        let buf = draw(&app, 100, 30);
        for style in backend_cells(&buf, &full(CLIENT), &format!("backend v{CLIENT}")) {
            assert!(
                matches!(style.fg, None | Some(Color::Reset)),
                "no colour when they agree: {style:?}"
            );
            assert!(style.add_modifier.contains(Modifier::DIM), "{style:?}");
        }
    }
    for backend in [Some("3.99"), None] {
        let _g = pinned(backend);
        let shown = backend.unwrap_or("?.??");
        let buf = draw(&app, 100, 30);
        let tag = format!("ham v{CLIENT} | backend v{shown}");
        for style in backend_cells(&buf, &tag, &format!("backend v{shown}")) {
            assert_eq!(style.fg, Some(Color::Yellow), "{backend:?}: {style:?}");
            assert!(
                style.add_modifier.contains(Modifier::BOLD),
                "{backend:?}: {style:?}"
            );
        }
        // The client half stays dim.
        let first = buf[(100 - u16::try_from(tag.chars().count()).unwrap(), 0)].style();
        assert!(matches!(first.fg, None | Some(Color::Reset)), "{first:?}");
        assert!(first.add_modifier.contains(Modifier::DIM));
    }
}

#[test]
fn a_screen_with_nothing_drawn_still_has_the_tag() {
    let _g = pinned(Some(CLIENT));
    // The dashboard of a run whose first read has not arrived draws nothing of its own on some paths; the tag is still there.
    let mut app = new_app();
    app.open_run(RUN);
    let row = row_text(&draw(&app, 100, 30), 0);
    assert!(row.ends_with(&full(CLIENT)), "{row:?}");
}

#[test]
fn a_frame_too_small_for_any_form_draws_no_tag() {
    let _g = pinned(Some(CLIENT));
    let row = row_text(&draw(&new_app(), 8, 6), 0);
    assert!(!row.contains("v4") && !row.contains('|'), "{row:?}");
}

// ---- ham --version ----------------------------------------------------------------------------------------------------------------

fn run_ham(args: &[&str]) -> std::process::Output {
    // No configuration, no credentials and no network: the flag is answered before any of them is read.
    std::process::Command::new(env!("CARGO_BIN_EXE_ham"))
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn version_and_dash_v_print_exactly_one_line_and_exit_0() {
    for flag in ["--version", "-V"] {
        let out = run_ham(&[flag]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("ham v{CLIENT}\n"),
            "{flag}"
        );
        assert!(
            out.stderr.is_empty(),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn version_is_only_the_flag_by_itself() {
    let out = run_ham(&["--version", "extra"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&format!("ham v{CLIENT}")));
}

// ---- ham doctor --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_versions_check_says_what_the_two_ends_run() {
    let ok = doctor::versions_check("4.01", Some("4.01"));
    assert_eq!((ok.name, ok.pass, ok.warn), ("versions", true, false));
    assert_eq!(ok.detail, "ham v4.01 | backend v4.01");

    let older = doctor::versions_check("4.01", Some("3.99"));
    assert_eq!(
        (older.name, older.pass, older.warn),
        ("versions", true, true),
        "advice, not a failure"
    );
    assert_eq!(
        older.detail,
        "ham v4.01 | backend v3.99; backend older than this ham"
    );

    let newer = doctor::versions_check("4.01", Some("4.02"));
    assert!(newer.pass && newer.warn);
    assert_eq!(
        newer.detail,
        "ham v4.01 | backend v4.02; backend newer than this ham"
    );

    let major = doctor::versions_check("10.01", Some("9.99"));
    assert!(
        major.detail.ends_with("backend older than this ham"),
        "{major:?}: versions compare as numbers, not text"
    );

    let unknown = doctor::versions_check("4.01", None);
    assert!(unknown.pass && unknown.warn);
    assert!(
        unknown.detail.starts_with("ham v4.01 | backend v?.??; "),
        "{unknown:?}"
    );
    assert!(unknown.detail.contains("reports no version"), "{unknown:?}");
    assert_eq!(unknown.level(), "warn");
}

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell; nextest runs each test alone
#[tokio::test]
async fn doctor_lists_the_versions_after_the_api_check() {
    let _g = pinned(None);
    let fake = Fake::start().await;
    let d = doctor::checks(&api(&fake), &fake.endpoint, "p").await;
    let names: Vec<&str> = d.checks.iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        [
            "identity",
            "api",
            "versions",
            "catalog",
            "git-host",
            "allowlist"
        ],
        "{:?}",
        d.checks
    );
    let v = d.checks.iter().find(|c| c.name == "versions").unwrap();
    assert_eq!(
        v.detail,
        format!("ham v{CLIENT} | backend v{CLIENT}"),
        "its own signed calls filled the cell"
    );
    assert!(!v.warn);

    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value("3.99".into());
    let d = doctor::checks(&api(&fake), &fake.endpoint, "p").await;
    let v = d.checks.iter().find(|c| c.name == "versions").unwrap();
    assert!(v.warn && v.pass, "{v:?}");
    assert!(
        v.detail.contains("backend v3.99") && v.detail.contains("backend older than this ham"),
        "{v:?}"
    );

    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Absent;
    let d = doctor::checks(&api(&fake), &fake.endpoint, "p").await;
    let v = d.checks.iter().find(|c| c.name == "versions").unwrap();
    assert!(v.warn && v.detail.contains("backend v?.??"), "{v:?}");
    let out = ham::output::Output::capture();
    let code = doctor::report(&d, false, &out);
    let printed = out.finish(code);
    assert_eq!(printed.code, 0, "a version warning never fails the doctor");
    assert!(
        printed
            .stdout_text()
            .contains(&format!("WARN versions ham v{CLIENT} | backend v?.??")),
        "{}",
        printed.stdout_text()
    );
}
