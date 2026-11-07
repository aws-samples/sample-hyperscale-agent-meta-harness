//! The ham mascot beside the app name: a title band on every screen of a terminal that is more than 30 rows tall and at least 72 columns
//! wide. On a terminal of 33 rows or more the band is five rows: the mascot's head (8 columns by 5 rows: the character, a flame under its legs
//! and a puff of smoke) stands at the left, the title starts on band row 0 from column 10 and the version tag stays at the right end of row 0;
//! the body of the screen starts on row 5. On a terminal of 31 or 32 rows the band is three rows with the short head (8 by 3) and the body
//! starts on row 3. At 30 rows or fewer, or under 72 columns (80x24, 100x30, 120x30 included), nothing changes: one title row and no mascot
//! beside the name. The band is a value injected into `App` (`Head`), so these tests choose it and never read the process environment.
//!
//! The strongest check is an oracle: every screen drawn with the band on a terminal `h` rows tall equals the same screen drawn without the
//! band on a terminal `h - N` rows tall, shifted down by `N` rows below the title row, where `N` is the band's rows less one (2 for the
//! three-row band, 4 for the five-row band). It holds for all seven screens (and the tag explorer), in both looks, so the geometry cannot
//! drift from the layout the screens always had.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::tui::chat::{Areas, areas_capped, areas_named};
use ham::tui::head::{
    BAND_ROWS, HEAD_PREFIX, Head, MIN_COLS, MIN_ROWS, SHORT_BAND_ROWS, TALL_ROWS, band_for,
    band_rows_for, split,
};
use ham::tui::mascot::{self, Form, Look};
use ham::tui::version_tag::{
    self, GAP, LONG_TAG, TITLE_KEEP, for_frame, for_frame_at, reserve, reserve_at, split_tag,
};
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham::version::{self, CLIENT};
use harness_protocol::api::v1::{ArtifactView, EventsPage, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

/// The tests that pin the process-wide backend cell run one at a time.
static CELL: Mutex<()> = Mutex::new(());

fn pinned(backend: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = CELL.lock().unwrap_or_else(PoisonError::into_inner);
    version::set_backend_for_test(backend);
    guard
}

const LOOKS: [Look; 2] = [Look::Blocks, Look::Ascii];

fn head(band: bool, look: Look) -> Head {
    Head {
        band,
        empty_table: true,
        look,
    }
}

fn banded(mut app: App, look: Look) -> App {
    app.set_head(head(true, look));
    app
}

fn compact(mut app: App, look: Look) -> App {
    app.set_head(head(false, look));
    app
}

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

/// The symbols of row `y`, one string, every cell one character.
fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// The rows of the band on a terminal `h` rows tall (the rule, written out here on its own): 5 from 33 rows, else 3.
fn band_rows(h: u16) -> u16 {
    if h >= 33 { 5 } else { 3 }
}

/// The form of the mascot in a band of `rows` rows.
fn band_form(rows: u16) -> Form {
    if rows == 5 {
        Form::Head
    } else {
        Form::HeadShort
    }
}

/// The head of the mascot as ratatui draws it alone in a band of `rows` rows: 8 x `rows` cells, with their styles, the still picture.
fn head_cells(look: Look, rows: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(8, rows)).unwrap();
    t.draw(|f| {
        f.render_widget(
            ratatui::widgets::Paragraph::new(mascot::lines(band_form(rows), look)),
            f.area(),
        )
    })
    .unwrap();
    t.backend().buffer().clone()
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

// ---- the rule -----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_band_is_on_from_31_rows_and_72_columns_and_five_rows_tall_from_33_when_the_switch_allows_it()
{
    assert_eq!(
        (
            MIN_ROWS,
            MIN_COLS,
            BAND_ROWS,
            SHORT_BAND_ROWS,
            TALL_ROWS,
            HEAD_PREFIX
        ),
        (31, 72, 5, 3, 33, 10)
    );
    let on = head(true, Look::Blocks);
    let off = head(false, Look::Blocks);
    // (width, height, rows of the band): 0 is no band.
    let table: [(u16, u16, u16); 20] = [
        (100, 31, 3),
        (72, 31, 3),
        (100, 32, 3),
        (72, 32, 3),
        (100, 33, 5),
        (72, 33, 5),
        (100, 30, 0),
        (72, 30, 0),
        (71, 31, 0),
        (71, 33, 0),
        (71, 60, 0),
        (80, 24, 0),
        (120, 30, 0),
        (120, 40, 5),
        (200, 60, 5),
        (0, 0, 0),
        (0, 40, 0),
        (100, 0, 0),
        (u16::MAX, u16::MAX, 5),
        (u16::MAX, 32, 3),
    ];
    for (w, h, rows) in table {
        assert_eq!(band_rows_for(w, h, on), rows, "{w}x{h} with the switch on");
        assert_eq!(band_for(w, h, on), rows > 0, "{w}x{h} with the switch on");
        assert_eq!(band_rows_for(w, h, off), 0, "{w}x{h} with the switch off");
        assert!(!band_for(w, h, off), "{w}x{h} with the switch off");
    }
}

#[test]
fn the_decision_does_not_depend_on_the_look() {
    for (w, h) in [
        (100, 31),
        (100, 30),
        (72, 31),
        (71, 31),
        (100, 33),
        (100, 32),
    ] {
        assert_eq!(
            band_rows_for(w, h, head(true, Look::Blocks)),
            band_rows_for(w, h, head(true, Look::Ascii)),
            "{w}x{h}"
        );
    }
}

#[test]
fn the_split_gives_the_title_and_the_area_the_screens_lay_themselves_out_in() {
    let on = head(true, Look::Blocks);
    // On: the title starts at column 10 of band row 0, the body at row 3. The area the screens lay out in starts two rows down, so that its
    // first row (the one-row title every screen has always had) is the last row of the band and its second row is row 3.
    let s = split(Rect::new(0, 0, 100, 31), on);
    assert!(s.band);
    assert_eq!(s.rows, 3);
    assert_eq!(s.left, 10);
    assert_eq!(s.title, Rect::new(10, 0, 90, 1));
    assert_eq!(s.rest, Rect::new(0, 3, 100, 28));
    assert_eq!(s.area, Rect::new(0, 2, 100, 29));
    // 32 rows keep the three-row band; 33 rows are the first with five: the body starts on row 5, the area four rows down, and the rows below
    // the band are the 28 rows that 31 rows had below the three-row band.
    let s = split(Rect::new(0, 0, 100, 32), on);
    assert_eq!(
        (s.rows, s.rest, s.area),
        (3, Rect::new(0, 3, 100, 29), Rect::new(0, 2, 100, 30))
    );
    let t = split(Rect::new(0, 0, 100, 33), on);
    assert!(t.band);
    assert_eq!(t.rows, 5);
    assert_eq!(t.left, 10);
    assert_eq!(t.title, Rect::new(10, 0, 90, 1));
    assert_eq!(t.rest, Rect::new(0, 5, 100, 28));
    assert_eq!(t.area, Rect::new(0, 4, 100, 29));
    assert_eq!(t.cap_base(), 28);
    // Off: the title is the first row, the rest the others, and the screens lay out in the whole frame, as they always did.
    let c = split(Rect::new(0, 0, 100, 30), on);
    assert!(!c.band);
    assert_eq!(c.rows, 0);
    assert_eq!(c.left, 0);
    assert_eq!(c.title, Rect::new(0, 0, 100, 1));
    assert_eq!(c.rest, Rect::new(0, 1, 100, 29));
    assert_eq!(c.area, Rect::new(0, 0, 100, 30));
    // The frame need not start at the origin.
    let o = split(Rect::new(5, 7, 100, 31), on);
    assert_eq!(o.title, Rect::new(15, 7, 90, 1));
    assert_eq!(o.rest, Rect::new(5, 10, 100, 28));
    assert_eq!(o.area, Rect::new(5, 9, 100, 29));
    let o5 = split(Rect::new(5, 7, 100, 33), on);
    assert_eq!(o5.title, Rect::new(15, 7, 90, 1));
    assert_eq!(o5.rest, Rect::new(5, 12, 100, 28));
    assert_eq!(o5.area, Rect::new(5, 11, 100, 29));
    // A frame of no size or of the largest size does not overflow.
    for frame in [
        Rect::new(0, 0, 0, 0),
        Rect::new(0, 0, u16::MAX, u16::MAX),
        Rect::new(u16::MAX - 1, u16::MAX - 1, 2, 2),
    ] {
        let s = split(frame, on);
        assert!(s.title.right() >= s.title.x && s.rest.bottom() >= s.rest.y);
    }
}

// ---- the tag does not change ----------------------------------------------------------------------------------------------------------

#[test]
fn the_band_never_changes_the_form_of_the_version_tag() {
    let _g = pinned(Some(CLIENT));
    // The long form is 25 columns, and the gate is the prefix, the columns the title keeps, the gap and the long form: 10 + 36 + 1 + 25.
    assert_eq!(
        LONG_TAG,
        format!("ham v{CLIENT} | backend v{CLIENT}").chars().count()
    );
    assert_eq!(
        usize::from(MIN_COLS),
        usize::from(HEAD_PREFIX) + TITLE_KEEP + GAP + LONG_TAG
    );
    for w in 72..=400u16 {
        for backend in [None, Some(CLIENT), Some("3.99")] {
            assert_eq!(
                for_frame_at(CLIENT, backend, usize::from(w), usize::from(HEAD_PREFIX)),
                for_frame(CLIENT, backend, usize::from(w)),
                "width {w}"
            );
        }
        assert_eq!(reserve_at(w, HEAD_PREFIX), reserve(w), "width {w}");
    }
    // Nothing left of the prefix changes the function at all.
    for w in 0..=120u16 {
        assert_eq!(reserve_at(w, 0), reserve(w), "width {w}");
        assert_eq!(
            for_frame_at(CLIENT, Some(CLIENT), usize::from(w), 0),
            for_frame(CLIENT, Some(CLIENT), usize::from(w)),
            "width {w}"
        );
    }
    // Under 72 columns the prefix would shorten the tag, which is why the band is not drawn there.
    let long = for_frame_at(CLIENT, Some(CLIENT), 71, 10).unwrap().width();
    assert!(long < LONG_TAG, "at 71 columns the tag would drop a form");
}

#[test]
fn the_tag_overlay_starts_its_clip_after_the_prefix_and_leaves_the_mascot_alone() {
    let _g = pinned(Some(CLIENT));
    let area = Rect::new(5, 7, 100, 31);
    let mut buf = Buffer::empty(Rect::new(0, 0, 120, 50));
    // The mascot's ASCII head at the left of the frame, a long title from the prefix on, and nothing else.
    for (i, text) in mascot::plain(Form::HeadShort, Look::Ascii)
        .iter()
        .enumerate()
    {
        buf.set_string(5, 7 + i as u16, text, ratatui::style::Style::default());
    }
    buf.set_string(15, 7, "T".repeat(90), ratatui::style::Style::default());
    version_tag::overlay_at(&mut buf, area, 10, CLIENT, Some(CLIENT));
    let tag = format!("ham v{CLIENT} | backend v{CLIENT}");
    let first: String = (5..105).map(|x| buf[(x, 7)].symbol()).collect();
    assert!(first.ends_with(&tag), "{first:?}");
    assert!(
        first.starts_with(" ,----. "),
        "the mascot is untouched: {first:?}"
    );
    // The title is cut one cell before the gap with an ellipsis.
    let gap_start = 105 - tag.chars().count() as u16 - 1;
    assert_eq!(buf[(gap_start - 1, 7)].symbol(), "\u{2026}");
    assert_eq!(buf[(gap_start - 2, 7)].symbol(), "T");
    assert_eq!(buf[(gap_start, 7)].symbol(), " ");
    // The other two rows of the mascot are untouched too.
    let second: String = (5..13).map(|x| buf[(x, 8)].symbol()).collect();
    assert_eq!(second, mascot::plain(Form::HeadShort, Look::Ascii)[1]);
}

// ---- the chat layout ------------------------------------------------------------------------------------------------------------------

/// The layout of the run screen before the band existed, copied here so that the shared function can be compared with it.
#[allow(clippy::too_many_arguments)]
fn old_areas_named(
    area: Rect,
    banner: bool,
    pending_rows: u16,
    input_rows: u16,
    notice: bool,
    reserved_rows: u16,
    name: bool,
) -> Areas {
    fn take(rem: &mut u16, want: u16) -> u16 {
        let got = want.min(*rem);
        *rem -= got;
        got
    }
    let mut rem = area.height;
    let footer_h = take(&mut rem, 1);
    let header_h = take(&mut rem, 1);
    let reserve = take(&mut rem, 3);
    let box_rows = input_rows.clamp(1, (area.height / 3).max(1));
    let input_h = take(&mut rem, box_rows + 2);
    let reserved_h = take(&mut rem, reserved_rows);
    let notice_h = take(&mut rem, u16::from(notice));
    let banner_h = take(&mut rem, u16::from(banner));
    let pending_h = take(&mut rem, pending_rows.min(3));
    let name_h = take(&mut rem, u16::from(name));
    let transcript_h = reserve + rem;
    let mut y = area.y;
    let mut next = |h: u16| {
        let r = Rect::new(area.x, y, area.width, h);
        y += h;
        r
    };
    Areas {
        header: next(header_h),
        name: next(name_h),
        banner: next(banner_h),
        transcript: next(transcript_h),
        pending: next(pending_h),
        reserved: next(reserved_h),
        input: next(input_h),
        notice: next(notice_h),
        footer: next(footer_h),
    }
}

#[test]
fn the_chat_layout_is_what_it_was_for_every_height_and_every_combination() {
    for w in [1u16, 20, 80, 200] {
        for h in 1..=60u16 {
            for banner in [false, true] {
                for pending in [0u16, 1, 3, 5] {
                    for input in [1u16, 2, 5, 30] {
                        for notice in [false, true] {
                            for reserved in [0u16, 2, 5] {
                                for name in [false, true] {
                                    let area = Rect::new(0, 0, w, h);
                                    let old = old_areas_named(
                                        area, banner, pending, input, notice, reserved, name,
                                    );
                                    let new = areas_named(
                                        area, banner, pending, input, notice, reserved, name,
                                    );
                                    assert_eq!(
                                        new, old,
                                        "{w}x{h} {banner} {pending} {input} {notice} {reserved} {name}"
                                    );
                                    let capped = areas_capped(
                                        area, h, banner, pending, input, notice, reserved, name,
                                    );
                                    assert_eq!(capped, old, "capped by its own height: {w}x{h}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn in_the_band_the_message_box_keeps_a_third_of_the_rows_below_the_band() {
    // 32 rows: the band takes 3, so 29 rows are left; a third is 9 rows of text and its two borders. The area the layout runs in has 30
    // rows (it starts two rows down), and the cap is told to be the 29.
    let area = Rect::new(0, 2, 100, 30);
    let capped = areas_capped(area, 29, false, 0, 40, false, 0, false);
    assert_eq!(capped.input.height, 9 + 2);
    let by_area = areas_named(area, false, 0, 40, false, 0, false);
    assert_eq!(
        by_area.input.height,
        10 + 2,
        "a third of the area's own 30 rows"
    );
}

// ---- every screen -----------------------------------------------------------------------------------------------------------------------

fn new_app() -> App {
    App::new_direct("n".into(), None, DirectSettings::default())
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

fn run_id(n: usize) -> String {
    format!("01M5A0000000000000{n:08}")
}

fn runs(n: usize) -> Vec<RunSummary> {
    (0..n)
        .map(|i| {
            serde_json::from_value(json!({
                "runId": run_id(i), "status": "complete", "phase": "planning",
                "target": {"mode": "s3-only"}, "createdAt": "2026-10-16T08:00:00Z",
                "ownerPrincipalArn": "arn:aws:sts::111122223333:assumed-role/Admin/tester"
            }))
            .unwrap()
        })
        .collect()
}

/// Every screen of the TUI, by the part of its title that is whole at the widths of these tests, the tag explorer included. Each call
/// builds new apps, so a test can draw the same case twice.
fn screens() -> Vec<(&'static str, App, String)> {
    let mut out: Vec<(&'static str, App, String)> = Vec::new();
    out.push(("runs", new_app(), "ham · Runs".into()));

    let mut listed = new_app();
    listed.update(Msg::Runs(Ok(runs(60))));
    out.push(("runs with 60 runs", listed, "ham · Runs".into()));

    let mut composer = new_app();
    composer.handle_key(key(KeyCode::Char('n')));
    out.push(("new run", composer, "ham · New run · model: ".into()));

    // The composer with the longest message it shows: three lines under the Submit line.
    let mut message = new_app();
    message.handle_key(key(KeyCode::Char('n')));
    message.say(&"m".repeat(240));
    out.push(("new run with a message", message, "ham · New run".into()));

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

    let mut named = dashboard();
    named.update(Msg::RunName {
        run_id: RUN.into(),
        name: Some((
            "Percent change function for the calculator".into(),
            Some("ai".into()),
        )),
    });
    out.push(("run with a name", named, format!("ham · Run {RUN}")));

    let mut reply = stuck();
    reply.handle_key(key(KeyCode::Char('p')));
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

    let mut settings = new_app();
    settings.handle_key(key(KeyCode::Char('s')));
    out.push(("settings", settings, "ham · Settings".into()));

    let mut tags = new_app();
    tags.handle_key(key(KeyCode::Char('t')));
    out.push(("tags", tags, "ham · Tags".into()));
    out
}

/// The title the screen was given, cut to the room the band leaves it: `width` less the prefix less the tag and its gap, one cell less for
/// the ellipsis a long title ends in.
fn title_prefix(title: &str, width: u16) -> String {
    let room = usize::from(width) - usize::from(HEAD_PREFIX) - LONG_TAG - GAP;
    title.chars().take(room.saturating_sub(1)).collect()
}

#[test]
fn every_screen_in_the_band_is_the_compact_screen_n_rows_shorter_under_the_band() {
    let _g = pinned(None);
    for look in LOOKS {
        // 31 and 32 rows have the three-row band (N = 2); 33 rows and more have the five-row band (N = 4).
        for (w, h) in [
            (100u16, 31u16),
            (72, 31),
            (100, 32),
            (100, 33),
            (72, 33),
            (120, 40),
            (200, 60),
            (100, 45),
        ] {
            let rows = band_rows(h);
            let n = rows - 1;
            let band_cases = screens();
            let flat_cases = screens();
            assert_eq!(band_cases.len(), flat_cases.len());
            for ((name, band_app, title), (_, flat_app, _)) in
                band_cases.into_iter().zip(flat_cases)
            {
                let with = buffer(&banded(band_app, look), w, h);
                let without = buffer(&compact(flat_app, look), w, h - n);
                let at = format!("{name} {look:?} {w}x{h}");
                // The body: rows `rows` and below are rows 1 and below of the screen `n` rows shorter. Same symbols, same styles.
                for y in 1..h - n {
                    for x in 0..w {
                        assert_eq!(
                            with[(x, y + n)],
                            without[(x, y)],
                            "{at}: cell ({x},{}) is not cell ({x},{y}) of the compact screen\n{}",
                            y + n,
                            (0..h).map(|r| row(&with, r)).collect::<Vec<_>>().join("\n")
                        );
                    }
                }
                // The mascot: its rows at columns 0 to 7, as ratatui draws the head alone.
                let art = head_cells(look, rows);
                for y in 0..rows {
                    for x in 0..8u16 {
                        assert_eq!(with[(x, y)], art[(x, y)], "{at}: mascot cell ({x},{y})");
                    }
                }
                // Nothing else in the band but the title on row 0 and the tag at its end.
                for y in 0..rows {
                    for x in 8..10u16 {
                        assert_eq!(with[(x, y)].symbol(), " ", "{at}: ({x},{y}) is the gap");
                    }
                }
                for y in 1..rows {
                    for x in 8..w {
                        assert_eq!(with[(x, y)].symbol(), " ", "{at}: ({x},{y}) is empty");
                    }
                }
                // Row 0: the title from column 10, the tag at the right end.
                let zero = row(&with, 0);
                let after_art: String = zero.chars().skip(10).collect();
                assert!(
                    after_art.starts_with(&title_prefix(&title, w)),
                    "{at}: {zero:?} does not start its title with {title:?}"
                );
                let tag = format!("ham v{CLIENT} | backend v?.??");
                assert!(zero.ends_with(&tag), "{at}: {zero:?}");
                assert_eq!(zero.chars().count(), usize::from(w), "{at}");
                assert!(split_tag(&zero).1.is_some(), "{at}");
            }
        }
    }
}

#[test]
fn below_the_gate_every_screen_is_what_it_was() {
    let _g = pinned(None);
    for look in LOOKS {
        for (w, h) in [
            (100u16, 30u16),
            (80, 24),
            (120, 30),
            (71, 31),
            (71, 33),
            (60, 40),
            (100, 24),
            (72, 30),
        ] {
            for ((name, with_switch, title), (_, without, _)) in
                screens().into_iter().zip(screens())
            {
                let a = buffer(&banded(with_switch, look), w, h);
                let b = buffer(&compact(without, look), w, h);
                assert_eq!(a, b, "{name} {look:?} {w}x{h}");
                // The title is the first thing on row 0 and no mascot stands beside it.
                let zero = row(&a, 0);
                assert!(
                    zero.starts_with(&title_start(&title)),
                    "{name} {w}x{h}: {zero:?}"
                );
            }
        }
    }
}

/// The part of a title that is whole at every width above: at 60 columns the room is narrower than some titles.
fn title_start(title: &str) -> String {
    title.chars().take(18).collect()
}

#[test]
fn the_two_looks_have_the_same_geometry() {
    let _g = pinned(None);
    for (w, h) in [(100u16, 31u16), (100, 33), (200, 60)] {
        for ((name, a, _), (_, b, _)) in screens().into_iter().zip(screens()) {
            let blocks = buffer(&banded(a, Look::Blocks), w, h);
            let ascii = buffer(&banded(b, Look::Ascii), w, h);
            for y in 0..h {
                for x in 0..w {
                    if x < 8 && y < band_rows(h) {
                        continue;
                    }
                    assert_eq!(blocks[(x, y)], ascii[(x, y)], "{name} {w}x{h}: ({x},{y})");
                }
            }
        }
    }
}

#[test]
fn a_frame_of_any_size_draws_without_a_fault_in_either_mode() {
    let _g = pinned(None);
    for look in LOOKS {
        for (w, h) in [
            (20u16, 24u16),
            (40, 10),
            (71, 60),
            (72, 31),
            (73, 32),
            (72, 33),
            (73, 34),
            (300, 120),
        ] {
            for (name, app, _) in screens() {
                let _ = (name, buffer(&banded(app, look), w, h));
            }
        }
    }
}

// ---- the run screen --------------------------------------------------------------------------------------------------------------------

#[test]
fn the_run_header_fits_beside_the_mascot_and_the_tag() {
    let _g = pinned(None);
    // 84 columns: 10 for the mascot, 26 for the tag and its gap, 48 for the header, which is exactly the title and the state of a
    // run that needs the user. It gives up its model and its tail and keeps the state.
    for look in LOOKS {
        let with = buffer(&banded(stuck(), look), 84, 31);
        let zero = row(&with, 0);
        assert!(
            zero.contains(&format!("ham · Run {RUN} · needs you")),
            "{zero:?}"
        );
        assert!(
            zero.ends_with(&format!("ham v{CLIENT} | backend v?.??")),
            "{zero:?}"
        );
        assert!(!zero.contains('\u{2026}'), "{zero:?}");
    }
    // 72 columns: the header has 36, so it is the run id and then cut; the tag is whole and the row is 72 cells.
    let at72 = buffer(&banded(stuck(), Look::Blocks), 72, 31);
    let zero = row(&at72, 0);
    assert_eq!(zero.chars().count(), 72);
    assert!(
        zero.ends_with(&format!("ham v{CLIENT} | backend v?.??")),
        "{zero:?}"
    );
    assert!(
        zero.chars()
            .skip(10)
            .collect::<String>()
            .starts_with("ham · Run "),
        "{zero:?}"
    );
}

/// The text of row 0 between the mascot and the version tag, with the gap trimmed: what the run screen's header drew. `None` when the row
/// does not end in the tag.
fn header_of(zero: &str, tag: &str) -> Option<String> {
    let before = zero.strip_suffix(tag)?;
    Some(before.chars().skip(HEAD_PREFIX.into()).collect::<String>())
}

#[test]
fn the_run_header_gives_up_whole_items_beside_the_tag_and_never_ends_in_an_ellipsis() {
    let _g = pinned(None);
    let tag = format!("ham v{CLIENT} | backend v?.??");
    let id = format!("ham \u{b7} Run {RUN}");
    for (name, make) in [("dashboard", dashboard as fn() -> App), ("stuck", stuck)] {
        // What the header says when the room is wide: the id, the model, the state and the age of the last read. The header gives them
        // up from the end (the age first), the model last, so these are the only three lines it may draw at 84 columns and more.
        let wide = row(&buffer(&banded(make(), Look::Blocks), 200, 31), 0);
        let wide = header_of(&wide, &tag).unwrap();
        let rest = wide
            .trim_end()
            .strip_prefix(&format!("{id} \u{b7} "))
            .unwrap_or_else(|| panic!("{name}: {wide:?}"));
        let parts: Vec<&str> = rest.split(" \u{b7} ").collect();
        assert_eq!(parts.len(), 3, "{name}: model, state, age: {rest:?}");
        let forms = [
            wide.trim_end().to_string(),
            format!("{id} \u{b7} {} \u{b7} {}", parts[0], parts[1]),
            format!("{id} \u{b7} {}", parts[1]),
        ];
        let mut seen = [false; 3];
        for w in 84..=160u16 {
            // The room the header has beside the mascot and the tag: the width of the title row less the tag and its gap.
            let room = usize::from(w) - usize::from(HEAD_PREFIX) - reserve(w);
            let zero = row(&buffer(&banded(make(), Look::Blocks), w, 31), 0);
            let at = format!("{name} at {w} columns: {zero:?}");
            assert_eq!(zero.chars().count(), usize::from(w), "{at}");
            let drawn = header_of(&zero, &tag).unwrap_or_else(|| panic!("{at}"));
            // A whole item is dropped or kept: no ellipsis anywhere on the row, and the gap before the tag stays blank.
            assert!(!zero.contains('\u{2026}'), "{at}");
            assert!(drawn.ends_with(' '), "no gap before the tag: {at}");
            // It is the longest of the three lines that fits the room.
            let k = forms
                .iter()
                .position(|f| f.chars().count() <= room)
                .unwrap_or_else(|| panic!("no form fits {room} columns: {at}"));
            assert_eq!(drawn.trim_end(), forms[k], "room {room}: {at}");
            seen[k] = true;
        }
        // The sweep passes through all three, so the test would notice a header that is fitted too wide.
        assert_eq!(seen, [true; 3], "{name}: every form is drawn at some width");
    }
    // The row the reviewers read: at 100 columns the header has 64 and gives up the model and the age; at 110 it has 74 and keeps the model.
    for look in LOOKS {
        let at100 = row(&buffer(&banded(dashboard(), look), 100, 31), 0);
        assert_eq!(
            header_of(&at100, &tag).unwrap().trim_end(),
            format!("{id} \u{b7} active"),
            "{at100:?}"
        );
        let at110 = row(&buffer(&banded(dashboard(), look), 110, 31), 0);
        assert!(
            header_of(&at110, &tag)
                .unwrap()
                .trim_end()
                .ends_with("\u{b7} active"),
            "{at110:?}"
        );
        assert!(!at110.contains('\u{2026}'), "{at110:?}");
    }
}

#[test]
fn a_very_long_name_stays_on_its_row_below_the_band() {
    let _g = pinned(None);
    let named = || {
        let mut app = stuck();
        app.update(Msg::RunName {
            run_id: RUN.into(),
            name: Some(("N".repeat(200), Some("ai".into()))),
        });
        app
    };
    // The name row is the first row below the band: row 3 under the three-row band, row 5 under the five-row band.
    for (h, first) in [(31u16, 3u16), (33, 5)] {
        let buf = buffer(&banded(named(), Look::Blocks), 72, h);
        let name_row = row(&buf, first);
        assert!(name_row.starts_with("NNNN"), "{h}: {name_row:?}");
        assert!(
            name_row.trim_end().ends_with('\u{2026}'),
            "{h}: {name_row:?}"
        );
        assert_eq!(name_row.chars().count(), 72);
        // The band rows hold the mascot, nothing of the name.
        for y in 0..first {
            assert!(!row(&buf, y).contains("NNNN"), "{h}: row {y}");
        }
    }
}

#[test]
fn the_message_box_in_the_band_keeps_a_third_of_the_rows_below_the_band() {
    let _g = pinned(None);
    // 32 rows: the band takes 3, so 29 rows are left, and a third of them is 9 rows of text between the two borders. A box of twenty lines
    // is cut to that; the same box on a 30-row terminal without the band keeps a third of 30, which is 10.
    let box_height = |band: bool, h: u16| -> u16 {
        let mut app = new_app();
        app.set_run_opens_typing(true);
        app.open_run(RUN);
        app.update(pages(working()));
        // Twenty lines typed: a letter, then Ctrl-J, which adds a line.
        for _ in 0..20 {
            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        let app = if band {
            banded(app, Look::Blocks)
        } else {
            compact(app, Look::Blocks)
        };
        let buf = buffer(&app, 100, h);
        let rows: Vec<String> = (0..h).map(|y| row(&buf, y)).collect();
        let top = rows
            .iter()
            .position(|r| r.contains("Message to Cline"))
            .unwrap();
        let bottom = rows[top..].iter().position(|r| r.starts_with('└')).unwrap();
        u16::try_from(bottom + 1).unwrap()
    };
    assert_eq!(box_height(true, 32), 9 + 2);
    assert_eq!(box_height(false, 30), 10 + 2);
    // 34 rows with the five-row band leave the same 29 rows below the band.
    assert_eq!(box_height(true, 34), 9 + 2);
}

// ---- resizing ----------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_resize_across_the_gate_lays_the_next_draw_out_again_and_leaves_no_residue() {
    let _g = pinned(None);
    let sizes = [
        (100u16, 31u16),
        (100, 30),
        (100, 31),
        (100, 33),
        (100, 32),
        (72, 33),
        (71, 33),
        (72, 31),
        (71, 31),
        (72, 31),
        (200, 60),
        (80, 24),
        (100, 33),
        (100, 31),
    ];
    for look in LOOKS {
        for (name, app, _) in screens() {
            let app = banded(app, look);
            let mut t = Terminal::new(TestBackend::new(100, 31)).unwrap();
            for (w, h) in sizes {
                t.backend_mut().resize(w, h);
                t.draw(|f| app.draw(f)).unwrap();
                let fresh = buffer(&app, w, h);
                assert_eq!(
                    t.backend().buffer(),
                    &fresh,
                    "{name} {look:?} after {w}x{h}"
                );
            }
        }
    }
}

#[test]
fn the_runs_window_keeps_the_selected_run_in_view_when_the_window_changes_by_one_row() {
    let _g = pinned(None);
    let mut app = new_app();
    app.update(Msg::Runs(Ok(runs(300))));
    for _ in 0..200 {
        app.handle_key(key(KeyCode::Down));
    }
    let app = banded(app, Look::Blocks);
    let want = format!("▶ {}", run_id(200));
    for (w, h) in [
        (100u16, 31u16),
        (100, 30),
        (100, 31),
        (100, 33),
        (100, 32),
        (100, 40),
        (100, 30),
    ] {
        let buf = buffer(&app, w, h);
        let screen: Vec<String> = (0..h).map(|y| row(&buf, y)).collect();
        assert!(
            screen.iter().any(|r| r.contains(&want)),
            "{w}x{h}: the selected run is off the screen\n{}",
            screen.join("\n")
        );
    }
    // The window is 23 rows in the three-row band (31 - 3 - status - help - border - header), 23 in the five-row band at 33 rows (33 - 5 - ...)
    // and 24 at 30 rows.
    let in_band = (0..31)
        .filter(|&y| {
            let r = row(&buffer(&app, 100, 31), y);
            r.starts_with('│') && r.contains(&run_id(0)[..18])
        })
        .count();
    let compact_rows = (0..30)
        .filter(|&y| {
            let r = row(&buffer(&app, 100, 30), y);
            r.starts_with('│') && r.contains(&run_id(0)[..18])
        })
        .count();
    let tall = (0..33)
        .filter(|&y| {
            let r = row(&buffer(&app, 100, 33), y);
            r.starts_with('│') && r.contains(&run_id(0)[..18])
        })
        .count();
    assert_eq!((in_band, tall, compact_rows), (23, 23, 24));
}

// ---- the mouse ---------------------------------------------------------------------------------------------------------------------------

struct Mouse {
    base: Instant,
    ms: u64,
}

impl Mouse {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ms: 0,
        }
    }

    fn at(&mut self, app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Vec<Effect> {
        self.ms += 30;
        app.handle_mouse_at(
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
            self.base + Duration::from_millis(self.ms),
        )
    }

    fn click(&mut self, app: &mut App, x: u16, y: u16) -> Vec<Effect> {
        let mut out = self.at(app, MouseEventKind::Down(MouseButton::Left), x, y);
        out.extend(self.at(app, MouseEventKind::Up(MouseButton::Left), x, y));
        out
    }
}

#[test]
fn a_click_on_the_mascot_or_in_the_band_does_nothing() {
    let _g = pinned(None);
    for h in [31u16, 33, 40] {
        for (name, app, _) in screens() {
            let mut app = banded(app, Look::Blocks);
            let before = buffer(&app, 100, h);
            let mut m = Mouse::new();
            for y in 0..band_rows(h) {
                for x in [0u16, 3, 7, 9] {
                    let effects = m.click(&mut app, x, y);
                    assert!(
                        effects.is_empty(),
                        "{name} {h} rows: a click at ({x},{y}) gave {effects:?}"
                    );
                }
            }
            assert_eq!(
                buffer(&app, 100, h),
                before,
                "{name} {h} rows: a click in the band changed the screen"
            );
        }
    }
}

fn message(n: u32, text: &str) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: u64::from(n),
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({"turn": 1, "kind": "message", "text": text}),
        },
    }
}

fn tool(n: u32, title: &str) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: u64::from(n),
        ts: "2026-10-14T20:00:00Z".into(),
        event_type: EventType::ToolSummary,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({"toolCallId": format!("t{n}"), "title": title, "kind": "execute", "status": "completed"}),
        },
    }
}

fn pos_of(buf: &Buffer, needle: &str) -> (u16, u16) {
    for y in 0..buf.area.height {
        let r = row(buf, y);
        if let Some(at) = r.find(needle) {
            return (r[..at].chars().count() as u16, y);
        }
    }
    panic!("{needle:?} is not on the screen");
}

fn copies(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Copy(p) => Some(p.text().to_string()),
            _ => None,
        })
        .collect()
}

fn with_steps(n: u32) -> App {
    let mut app = dashboard();
    app.update(pages(
        (0..n)
            .map(|i| tool(100 + i, &format!("step {i:03}")))
            .collect(),
    ));
    app
}

#[test]
fn a_drag_to_a_band_row_scrolls_and_the_copy_holds_no_mascot() {
    let _g = pinned(None);
    // At 31 rows the band is three rows, at 40 it is five (the flame and the smoke stand in rows 3 and 4): a drag to any band row scrolls, and
    // none of the mascot's glyphs is in the copy.
    for (look, h) in LOOKS.into_iter().flat_map(|l| [(l, 31u16), (l, 40)]) {
        let mut app = banded(with_steps(120), look);
        let screen = buffer(&app, 100, h);
        let (x_last, y_last) = pos_of(&screen, "step 119");
        let (_, y_first) = pos_of(&screen, "step ");
        let pane_rows = usize::from(y_last - y_first) + 1;
        let mut m = Mouse::new();
        m.at(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            x_last + 7,
            y_last,
        );
        let mut effects = Vec::new();
        // The rows of the band below the title: 1 and 2, and for the five-row band 3 and 4 as well (the head's flame and smoke).
        let drag: &[u16] = if h >= 33 {
            &[4, 3, 2, 1, 2, 1]
        } else {
            &[2, 1, 2, 1]
        };
        for &y in drag {
            effects.extend(m.at(&mut app, MouseEventKind::Drag(MouseButton::Left), x_last, y));
        }
        let _ = buffer(&app, 100, h);
        effects.extend(m.at(&mut app, MouseEventKind::Up(MouseButton::Left), x_last, 1));
        let got = copies(&effects);
        assert_eq!(got.len(), 1, "{look:?} {h} rows: {effects:?}");
        let steps = got[0].lines().filter(|l| l.starts_with("step ")).count();
        assert!(
            steps > pane_rows,
            "{look:?} {h} rows: the copy holds {steps} rows, more than the {pane_rows} rows of the pane"
        );
        assert!(got[0].ends_with("step 119"), "{:?}", got[0]);
        for glyph in ['▀', '▄', '█', '@', '#', '\\', '%'] {
            assert!(
                !got[0].contains(glyph),
                "{look:?} {h} rows: the copy holds {glyph:?}"
            );
        }
    }
}

#[test]
fn a_selection_survives_a_resize_across_the_gate_as_the_same_text_or_is_dropped() {
    let _g = pinned(None);
    let mut app = banded(dashboard(), Look::Blocks);
    app.update(pages(vec![message(10, "Second answer with `code` in it.")]));
    let screen = buffer(&app, 100, 31);
    let (x0, y) = pos_of(&screen, "Second answer");
    let mut m = Mouse::new();
    m.at(&mut app, MouseEventKind::Down(MouseButton::Left), x0, y);
    m.at(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        x0 + 12,
        y,
    );
    m.at(&mut app, MouseEventKind::Up(MouseButton::Left), x0 + 12, y);
    app.resized();
    let after = buffer(&app, 100, 30);
    let reversed: String = (0..30u16)
        .flat_map(|yy| (9..100u16).map(move |x| (x, yy)))
        .filter(|&(x, yy)| {
            after[(x, yy)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
        .map(|(x, yy)| after[(x, yy)].symbol().to_string())
        .collect();
    assert!(
        reversed.is_empty() || reversed == "Second answer",
        "the selection became {reversed:?}"
    );
}

// ---- the open-anything box --------------------------------------------------------------------------------------------------------------

#[test]
fn the_open_anything_box_covers_the_mascot_and_the_tag_is_still_drawn_last() {
    let _g = pinned(None);
    for (look, h) in LOOKS.into_iter().flat_map(|l| [(l, 31u16), (l, 40)]) {
        let mut app = banded(new_app(), look);
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let buf = buffer(&app, 100, h);
        let zero = row(&buf, 0);
        assert!(zero.starts_with("┌ Open anything"), "{look:?}: {zero:?}");
        assert!(
            zero.trim_end()
                .ends_with(&format!("ham v{CLIENT} | backend v?.??")),
            "{look:?}: {zero:?}"
        );
        assert!(row(&buf, 1).starts_with('│'), "{look:?}");
        for (i, art) in mascot::plain(band_form(band_rows(h)), look)
            .iter()
            .enumerate()
        {
            let cells: String = row(&buf, i as u16).chars().take(8).collect();
            assert_ne!(
                &cells, art,
                "{look:?} {h} rows: the box left the mascot's row {i}"
            );
        }
    }
}

#[test]
fn the_tag_explorer_covers_the_frame_and_the_mascot_stands_over_it() {
    let _g = pinned(None);
    for h in [31u16, 33, 40] {
        let rows = band_rows(h);
        let mut app = banded(new_app(), Look::Blocks);
        app.handle_key(key(KeyCode::Char('t')));
        let buf = buffer(&app, 100, h);
        let art = head_cells(Look::Blocks, rows);
        for y in 0..rows {
            for x in 0..8u16 {
                assert_eq!(buf[(x, y)], art[(x, y)], "{h} rows: ({x},{y})");
            }
        }
        assert!(
            row(&buf, 0)
                .chars()
                .skip(10)
                .collect::<String>()
                .starts_with("ham · Tags"),
            "{h} rows"
        );
        // Its box starts on the first row below the band and its key line is the last row.
        assert!(row(&buf, rows).starts_with('┌'), "{h} rows");
        assert!(
            row(&buf, h - 1).contains("Esc"),
            "{h} rows: {:?}",
            row(&buf, h - 1)
        );
    }
}

// ---- the artifacts of a finished delivery keep their rows -----------------------------------------------------------------------------------

#[test]
fn the_band_costs_every_screen_exactly_its_rows_less_one_of_room() {
    let _g = pinned(None);
    // 31 rows with the three-row band show what 29 rows show without it, and 33 rows with the five-row band show what 29 rows show too:
    // the list of models of the composer is whole, the last rows of the key line are the last rows of the frame.
    for h in [31u16, 32, 33, 40] {
        let (_, composer, _) = screens()
            .into_iter()
            .find(|(n, _, _)| *n == "new run with a message")
            .unwrap();
        let buf = buffer(&banded(composer, Look::Blocks), 100, h);
        let screen: Vec<String> = (0..h).map(|y| row(&buf, y)).collect();
        assert!(
            screen.last().unwrap().contains("Esc cancel"),
            "{h} rows: {screen:#?}"
        );
        let text = screen.join("\n");
        for fragment in [
            "Prompt (Enter adds a line)",
            "Target (←→ changes)",
            "Model (↑↓ picks",
            "[ Submit ]",
        ] {
            assert!(text.contains(fragment), "{h} rows: {fragment}\n{text}");
        }
    }
}
