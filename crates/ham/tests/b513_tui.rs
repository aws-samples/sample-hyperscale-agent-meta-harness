//! The update notice on the screen. The terminal app looks at the update source for a newer release and, when `ham update` has
//! something to install, shows `v<version> available` one blank left of the version tag on the first row of every screen, bold and
//! reversed. The notice only indicates the release. A click on it hands the web page of the update source to the opener the app uses for
//! every link, and the app stays open; a source without a web page opens nothing. Either way one sentence says how to update: quit ham
//! and run `ham update`. The sentence has rows of its own under the screen, wrapped to the width, on every screen and over both
//! overlays, until the next key or press of a mouse button. The notice has no key, and `U` is no key of the app.
//!
//! These tests play the event loop: the look is answered by the test (`Msg::UpdateLook`), the opener is a stand-in program of the test,
//! and the git the app is told is never started. No test reads a host outside this machine.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::head::Head;
use ham::tui::keys::Mode;
use ham::tui::mascot::Look;
use ham::tui::mouse::Target;
use ham::tui::settings::{self, Job, Op};
use ham::tui::version_tag;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use ham::update::LookOutcome;
use ham::version::{self, CLIENT};
use ham_core::settings::Key;
use ham_core::update::DEFAULT_SOURCE;
use harness_protocol::api::v1::{ArtifactView, EventsPage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
/// The git the app is told for its look. No test starts it: the tests answer the look themselves.
const GIT: &str = "/usr/bin/git";
/// An update source with a web page, and that page.
const SOURCE: &str = "https://git.example.test/team/ham.git";
const PAGE: &str = "https://git.example.test/team/ham";
/// The opener the app is told (`HAMH_OPEN_COMMAND`). The app only names it in an effect.
const OPENER: &str = "/stand/in";
const HOW: &str = "To update, quit ham and run ham update.";

/// The tests that pin the process-wide backend cell run one at a time.
static CELL: Mutex<()> = Mutex::new(());

fn pinned(backend: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = CELL.lock().unwrap_or_else(PoisonError::into_inner);
    version::set_backend_for_test(backend);
    guard
}

/// The version `by` releases after this build's (`5.24` and 1 give `5.25`).
fn next(by: u32) -> String {
    let (major, minor) = CLIENT.split_once('.').unwrap();
    format!("{major}.{:02}", minor.parse::<u32>().unwrap() + by)
}

fn full_tag() -> String {
    format!("ham v{CLIENT} | backend v{CLIENT}")
}

/// The words of the notice for the release after this build's.
fn notice() -> String {
    format!("v{} available", next(1))
}

fn new_app() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_open_command(Some(OPENER.into()));
    app
}

fn looks(effects: &[Effect]) -> Vec<(u64, PathBuf)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::LookForUpdate {
                generation,
                program,
            } => Some((*generation, program.clone())),
            _ => None,
        })
        .collect()
}

/// The app is told a git for its look and its first look starts: the generation that look carries.
fn looking(mut app: App) -> (App, u64) {
    app.set_update_look(Some(PathBuf::from(GIT)));
    let first = looks(&app.update_look_first());
    assert_eq!(first.len(), 1, "one look when the app starts");
    assert_eq!(first[0].1, Path::new(GIT));
    (app, first[0].0)
}

fn answer(
    app: &mut App,
    generation: u64,
    outcome: LookOutcome,
    source: Option<&str>,
) -> Vec<Effect> {
    app.update(Msg::UpdateLook {
        generation,
        outcome,
        source: source.map(str::to_string),
    })
}

/// An app whose look found the release after this build's on `source`.
fn noticed_on(app: App, source: &str) -> App {
    let (mut app, generation) = looking(app);
    let effects = answer(
        &mut app,
        generation,
        LookOutcome::Newest(next(1)),
        Some(source),
    );
    assert_eq!(effects, vec![], "the answer of a look starts nothing");
    app
}

fn noticed(app: App) -> App {
    noticed_on(app, SOURCE)
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

/// The run screen in browse mode, where letters are hotkeys.
fn dashboard() -> App {
    let mut app = new_app();
    app.open_run(RUN);
    app.update(pages(working()));
    app
}

/// The run screen with the message box in front (typing mode).
fn typing() -> App {
    let mut app = dashboard();
    app.handle_key(key(KeyCode::Char('i')));
    assert_eq!(app.mode(), Mode::Typing);
    app
}

/// The run screen of a run that waits for the person.
fn stuck() -> App {
    let mut events = working();
    events.push(question());
    let mut app = new_app();
    app.open_run(RUN);
    app.update(pages(events));
    app
}

/// Every screen of the terminal app, with how its title starts (empty where the first row holds no title of its own).
fn screens() -> Vec<(&'static str, App, String)> {
    let mut out: Vec<(&'static str, App, String)> = Vec::new();
    out.push(("runs", new_app(), "ham · Runs".into()));

    let mut composer = new_app();
    composer.handle_key(key(KeyCode::Char('n')));
    out.push(("new run", composer, "ham · New run · model: ".into()));

    out.push(("run, browsing", dashboard(), format!("ham · Run {RUN}")));
    out.push(("run, typing", typing(), format!("ham · Run {RUN}")));

    let mut reply = stuck();
    reply.handle_key(key(KeyCode::Char('p')));
    out.push(("reply", reply, format!("ham · Run {RUN}")));

    let mut models = stuck();
    models.handle_key(key(KeyCode::Char('m')));
    out.push(("models", models, format!("ham · Run {RUN}")));

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
    out.push(("tag explorer", tags, String::new()));

    let mut open = new_app();
    open.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    out.push(("open anything", open, String::new()));
    out
}

fn draw(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

/// The words of the whole screen with one blank between them, so that a sentence the screen wrapped over two rows reads as one.
fn screen_text(app: &App, w: u16, h: u16) -> String {
    let buf = draw(app, w, h);
    (0..buf.area.height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The notice on the first row of a draw, read back by its shape; `None` when the row holds none.
fn notice_on(app: &App, w: u16, h: u16) -> Option<String> {
    let row = row_text(&draw(app, w, h), 0);
    let (_, notice, tag) = version_tag::split_row0(&row);
    assert!(tag.is_some(), "the tag is on the first row: {row:?}");
    notice.map(str::to_string)
}

/// The cells of the notice in the hit map of the last draw.
fn notice_cells(app: &App) -> Option<Rect> {
    let map = app.hit_map();
    let found: Vec<Rect> = map
        .regions()
        .iter()
        .filter(|r| r.target == Target::Update)
        .map(|r| r.rect)
        .collect();
    assert!(
        found.len() <= 1,
        "at most one region of the notice: {found:?}"
    );
    found.first().copied()
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

/// A click on the notice of a screen drawn at `w` by `h`: the button goes down on its second cell and comes up on its last one.
fn click_notice(app: &mut App, w: u16, h: u16) -> Vec<Effect> {
    draw(app, w, h);
    let cells = notice_cells(app).expect("the notice is a region of the mouse");
    let at = Instant::now();
    let down = app.handle_mouse_at(
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            cells.x + 1,
            cells.y,
        ),
        at,
    );
    assert_eq!(down, vec![], "the press alone does nothing");
    app.handle_mouse_at(
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            cells.x + cells.width - 1,
            cells.y,
        ),
        at,
    )
}

// ---- when the notice shows -----------------------------------------------------------------------------------------------------------

#[test]
fn b513_no_notice_without_a_look_and_when_there_is_nothing_to_install() {
    let _g = pinned(Some(CLIENT));
    // An app that was told no git (every app a test builds, a client outside a folder named bin): no look and no notice, whatever arrives.
    let mut app = new_app();
    assert_eq!(app.update_look_first(), vec![]);
    assert_eq!(looks(&app.tick()), vec![]);
    answer(&mut app, 0, LookOutcome::Newest(next(1)), Some(SOURCE));
    assert_eq!(notice_on(&app, 120, 30), None);
    draw(&app, 120, 30);
    assert_eq!(notice_cells(&app), None);

    // Before the first look ended.
    let (mut app, generation) = looking(new_app());
    assert_eq!(notice_on(&app, 120, 30), None);
    // The look was switched off, found no release, or failed.
    for outcome in [
        LookOutcome::Off,
        LookOutcome::NoRelease,
        LookOutcome::Failed,
    ] {
        let (mut app, generation) = looking(new_app());
        assert_eq!(answer(&mut app, generation, outcome.clone(), None), vec![]);
        assert_eq!(notice_on(&app, 120, 30), None, "{outcome:?}");
    }
    // The newest release is the one installed, or an older one: ham update has nothing to install.
    answer(
        &mut app,
        generation,
        LookOutcome::Newest(CLIENT.to_string()),
        Some(SOURCE),
    );
    assert_eq!(notice_on(&app, 120, 30), None);
    let (mut app, generation) = looking(new_app());
    answer(
        &mut app,
        generation,
        LookOutcome::Newest("0.01".to_string()),
        Some(SOURCE),
    );
    assert_eq!(notice_on(&app, 120, 30), None);

    // A newer release: the notice.
    let app = noticed(new_app());
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
}

#[test]
fn b513_the_notice_stands_left_of_the_tag_on_every_screen() {
    let _g = pinned(Some(CLIENT));
    let button = Modifier::BOLD | Modifier::REVERSED;
    for (name, app, title) in screens() {
        let app = noticed(app);
        for (w, h) in [(100u16, 30u16), (120, 40), (200, 60)] {
            let buf = draw(&app, w, h);
            let row = row_text(&buf, 0);
            let end = format!(" {} {}", notice(), full_tag());
            assert!(row.ends_with(&end), "{name} at {w}x{h}: {row:?}");
            assert!(
                row.starts_with(&title),
                "{name} at {w}x{h}: the title stays leftmost: {row:?}"
            );
            // The same words on every screen: the notice names no key, so it does not change with what the keyboard does.
            let (_, read, tag) = version_tag::split_row0(&row);
            assert_eq!(read, Some(notice().as_str()), "{name}");
            assert_eq!(tag, Some(full_tag().as_str()), "{name}");
            // Bold and reversed, so it reads as a button without colour; the blank on each side is not part of it.
            let width = u16::try_from(notice().chars().count()).unwrap();
            let x = w - u16::try_from(full_tag().chars().count()).unwrap() - 1 - width;
            for cell_x in x..x + width {
                assert!(
                    buf[(cell_x, 0)].modifier.contains(button),
                    "{name} at {w}x{h}, column {cell_x}"
                );
            }
            for cell_x in [x - 1, x + width] {
                assert_eq!(buf[(cell_x, 0)].symbol(), " ", "{name} column {cell_x}");
                assert!(
                    !buf[(cell_x, 0)].modifier.contains(Modifier::REVERSED),
                    "{name} column {cell_x}"
                );
            }
            // The cells of the notice are its region of the mouse, on every screen.
            assert_eq!(
                notice_cells(&app),
                Some(Rect::new(x, 0, width, 1)),
                "{name} at {w}x{h}"
            );
        }
    }
}

#[test]
fn b513_the_notice_shortens_before_it_goes_and_the_tag_stays() {
    let _g = pinned(Some(CLIENT));
    let app = noticed(new_app());
    let tag_of = |w: u16| {
        let row = row_text(&draw(&app, w, 24), 0);
        let (_, _, tag) = version_tag::split_row0(&row);
        tag.unwrap().to_string()
    };
    // The numbers are those of the room rule: 36 columns of title, a blank, the notice, a blank, the tag.
    let n = CLIENT.chars().count();
    let (long_tag, short_tag, bare_tag) = (2 * n + 17, 2 * n + 9, 2 * n + 1);
    let long = notice().chars().count();
    let at = |notice: usize, tag: usize| u16::try_from(36 + 1 + notice + 1 + tag).unwrap();
    assert_eq!(notice_on(&app, at(long, long_tag), 24), Some(notice()));
    assert_eq!(tag_of(at(long, long_tag)), full_tag());
    assert_eq!(notice_on(&app, at(long, long_tag) - 1, 24), Some(notice()));
    assert_eq!(
        tag_of(at(long, long_tag) - 1),
        format!("h v{CLIENT} | b v{CLIENT}"),
        "the tag takes its shorter form first"
    );
    assert_eq!(
        notice_on(&app, at(long, short_tag) - 1, 24).as_deref(),
        Some("update"),
        "then the notice does"
    );
    assert_eq!(tag_of(at(long, short_tag) - 1), full_tag());
    assert_eq!(
        notice_on(&app, at(6, bare_tag), 24).as_deref(),
        Some("update")
    );
    assert_eq!(tag_of(at(6, bare_tag)), format!("{CLIENT}|{CLIENT}"));
    // Narrower than that: the tag alone, and no region of the mouse.
    assert_eq!(notice_on(&app, at(6, bare_tag) - 1, 24), None);
    assert_eq!(tag_of(at(6, bare_tag) - 1), format!("{CLIENT}|{CLIENT}"));
    draw(&app, at(6, bare_tag) - 1, 24);
    assert_eq!(notice_cells(&app), None);
}

#[test]
fn b513_the_run_header_gives_up_its_tail_for_the_notice() {
    let _g = pinned(Some(CLIENT));
    let app = noticed(dashboard());
    let header = |w: u16| {
        let row = row_text(&draw(&app, w, 30), 0);
        let (title, read, tag) = version_tag::split_row0(&row);
        assert_eq!(read, Some(notice().as_str()), "{w}: {row:?}");
        assert_eq!(tag, Some(full_tag().as_str()), "{w}: {row:?}");
        title.to_string()
    };
    // 120 columns: the tag and the notice take 42 columns, and the whole header fits beside them.
    assert!(
        header(120).starts_with(&format!("ham · Run {RUN} · Claude Sonnet 5.5 · active")),
        "{:?}",
        header(120)
    );
    // 100 columns: the header has 58. It gives up the model, the first part of its tail, for the notice as it does for the tag;
    // the run id and the state stay whole and nothing is cut.
    assert_eq!(header(100), format!("ham · Run {RUN} · active"));
    // 80 columns: the title keeps its 36 columns, which hold the whole run id.
    assert!(
        header(80).starts_with(&format!("ham · Run {RUN}")),
        "{:?}",
        header(80)
    );
    // Without a notice the header at 100 columns still names the model: the room went to the notice, and only that.
    let plain = dashboard();
    let row = row_text(&draw(&plain, 100, 30), 0);
    let (title, _) = version_tag::split_tag(&row);
    assert!(
        title.starts_with(&format!("ham · Run {RUN} · Claude Sonnet 5.5 · active")),
        "{row:?}"
    );
}

#[test]
fn b513_the_notice_follows_the_backend_version_without_a_look() {
    let _g = pinned(None);
    let mut app = new_app();
    app.set_now(1_792_000_000);
    let mut app = noticed(app);
    let mut ticks = Vec::new();
    // The backend has not answered yet: ham update could not tell, so no notice.
    assert_eq!(notice_on(&app, 120, 30), None);
    ticks.extend(app.tick());
    // Its version arrives: the notice at the next draw.
    version::set_backend_for_test(Some(CLIENT));
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    ticks.extend(app.tick());
    // Someone updated the stage past the newest release the look named: ham update would refuse, so the notice goes.
    version::set_backend_for_test(Some(&next(2)));
    assert_eq!(notice_on(&app, 120, 30), None);
    ticks.extend(app.tick());
    // The backend runs the newest release and the client does not: the client is to install.
    version::set_backend_for_test(Some(&next(1)));
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    ticks.extend(app.tick());
    assert_eq!(looks(&ticks), vec![], "none of it starts a look");
}

#[test]
fn b513_the_notice_is_a_region_of_the_mouse_and_has_no_key() {
    let _g = pinned(Some(CLIENT));
    // Without a notice every region names its key, as before.
    let plain = new_app();
    draw(&plain, 120, 30);
    assert_eq!(notice_cells(&plain), None);
    assert!(plain.hit_map().unkeyed().is_empty());

    let mut app = noticed(new_app());
    let row = row_text(&draw(&app, 120, 30), 0);
    // The column of the notice: the cells before it, counted as characters (the title holds a character of two bytes).
    let before = &row[..row.find(&notice()).unwrap()];
    let x = u16::try_from(before.chars().count()).unwrap();
    let width = u16::try_from(notice().chars().count()).unwrap();
    assert_eq!(notice_cells(&app), Some(Rect::new(x, 0, width, 1)));
    {
        // The notice is the one region without a key: its click opens a web page.
        let map = app.hit_map();
        let unkeyed = map.unkeyed();
        assert_eq!(unkeyed.len(), 1, "{unkeyed:?}");
        assert_eq!(unkeyed[0].target, Target::Update);
    }
    // `U` is no key of the app: on the Runs list and in browse mode of the run screen it does nothing, and the app stays open.
    assert_eq!(app.handle_key(key(KeyCode::Char('U'))), vec![]);
    assert_eq!(app.screen(), Screen::Runs);
    let mut run = noticed(dashboard());
    assert_eq!(run.mode(), Mode::Browse);
    assert_eq!(run.handle_key(key(KeyCode::Char('U'))), vec![]);
    assert_eq!(run.screen(), Screen::Dashboard);
    assert_eq!(notice_on(&run, 120, 30), Some(notice()));
}

// ---- the click -----------------------------------------------------------------------------------------------------------------------

#[test]
fn b513_a_click_hands_the_page_to_the_opener_and_the_app_stays_open() {
    let _g = pinned(Some(CLIENT));
    let said = format!("Opened {PAGE}. {HOW}");
    let cases: Vec<(&str, App, Screen)> = vec![
        ("runs", noticed(new_app()), Screen::Runs),
        ("run, browsing", noticed(dashboard()), Screen::Dashboard),
        ("run, typing", noticed(typing()), Screen::Dashboard),
        (
            "settings",
            {
                let mut app = noticed(new_app());
                app.handle_key(key(KeyCode::Char('s')));
                app
            },
            Screen::Settings,
        ),
    ];
    for (name, mut app, screen) in cases {
        let effects = click_notice(&mut app, 200, 40);
        assert_eq!(
            effects,
            vec![Effect::OpenUpdatePage {
                program: OPENER.into(),
                url: PAGE.into(),
            }],
            "{name}: the page of the source goes to the opener, and nothing else happens"
        );
        assert_eq!(app.screen(), screen, "{name}: the app stays where it was");
        assert!(
            !screen_text(&app, 200, 40).contains(HOW),
            "{name}: nothing is said before the opener answered"
        );
        // The opener answered: one sentence where the screen shows a line.
        let effects = app.update(Msg::UpdatePage {
            url: PAGE.into(),
            result: Ok(()),
        });
        assert_eq!(effects, vec![], "{name}");
        let text = screen_text(&app, 200, 40);
        assert!(text.contains(&said), "{name}: {text}");
        assert_eq!(app.screen(), screen, "{name}");
        assert_eq!(
            notice_on(&app, 200, 40),
            Some(notice()),
            "{name}: the notice stays"
        );
    }
}

#[test]
fn b513_the_default_source_opens_the_page_of_the_public_repository() {
    let _g = pinned(Some(CLIENT));
    let mut app = noticed_on(new_app(), DEFAULT_SOURCE);
    assert_eq!(
        click_notice(&mut app, 160, 30),
        vec![Effect::OpenUpdatePage {
            program: OPENER.into(),
            url: "https://github.com/aws-samples/sample-hyperscale-agent-meta-harness".into(),
        }]
    );
    // Without `HAMH_OPEN_COMMAND` the program is the platform's opener, as for every link.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app = noticed_on(app, DEFAULT_SOURCE);
    let effects = click_notice(&mut app, 160, 30);
    let [Effect::OpenUpdatePage { program, .. }] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(program, ham::open::default_program());
}

#[test]
fn b513_a_source_without_a_web_page_opens_nothing_and_says_how_to_update() {
    let _g = pinned(Some(CLIENT));
    let said = format!("v{} is available. {HOW}", next(1));
    for source in [
        "file:///srv/mirror/ham.git",
        "https://user@git.example.test/team/ham.git",
    ] {
        let mut app = noticed_on(new_app(), source);
        assert_eq!(notice_on(&app, 200, 40), Some(notice()), "{source}");
        assert_eq!(
            click_notice(&mut app, 200, 40),
            vec![],
            "{source}: nothing is opened"
        );
        let text = screen_text(&app, 200, 40);
        assert!(text.contains(&said), "{source}: {text}");
        assert_eq!(app.screen(), Screen::Runs);
    }
    // The same on the run screen.
    let mut app = noticed_on(dashboard(), "file:///srv/mirror/ham.git");
    assert_eq!(click_notice(&mut app, 200, 40), vec![]);
    assert!(screen_text(&app, 200, 40).contains(&said));
}

#[test]
fn b513_an_opener_that_fails_says_so_and_how_to_update() {
    let _g = pinned(Some(CLIENT));
    let failure = "could not run /stand/in: No such file or directory (set HAMH_OPEN_COMMAND to a program that opens a URL)";
    let said = format!("could not open the link: {failure}. {HOW}");
    for mut app in [noticed(new_app()), noticed(dashboard())] {
        assert_eq!(click_notice(&mut app, 240, 40).len(), 1);
        let effects = app.update(Msg::UpdatePage {
            url: PAGE.into(),
            result: Err(failure.into()),
        });
        assert_eq!(effects, vec![], "nothing is copied and nothing is retried");
        let text = screen_text(&app, 240, 40);
        assert!(text.contains(&said), "{text}");
        assert_eq!(notice_on(&app, 240, 40), Some(notice()));
    }
}

#[test]
fn b513_a_press_that_does_not_end_on_the_notice_does_nothing() {
    let _g = pinned(Some(CLIENT));
    let mut app = noticed(new_app());
    draw(&app, 160, 30);
    let cells = notice_cells(&app).unwrap();
    let at = Instant::now();
    let down = |x: u16, y: u16| mouse(MouseEventKind::Down(MouseButton::Left), x, y);
    let up = |x: u16, y: u16| mouse(MouseEventKind::Up(MouseButton::Left), x, y);
    // Down on the notice, up on the row under it.
    assert_eq!(app.handle_mouse_at(down(cells.x, 0), at), vec![]);
    assert_eq!(app.handle_mouse_at(up(cells.x, 1), at), vec![]);
    // Down on the tag, up on the notice.
    assert_eq!(
        app.handle_mouse_at(down(cells.x + cells.width + 3, 0), at),
        vec![]
    );
    assert_eq!(app.handle_mouse_at(up(cells.x, 0), at), vec![]);
    assert!(!screen_text(&app, 160, 30).contains(HOW));
}

// ---- where the sentence of a click shows -----------------------------------------------------------------------------------------------

/// The page a click opens for the default source: the public repository.
const DEFAULT_PAGE: &str = "https://github.com/aws-samples/sample-hyperscale-agent-meta-harness";
/// An update source without a web page.
const MIRROR: &str = "file:///srv/mirror/ham.git";
/// What the opener says where the platform's program is not installed (a session without a browser).
const NO_OPENER: &str = "could not run xdg-open: No such file or directory (os error 2) (set HAMH_OPEN_COMMAND to a program that opens a URL)";

/// A sentence a click on the notice can end in: what happened, the update source, the opener's answer when the source has a page, and
/// the sentence.
type Sentence = (
    &'static str,
    &'static str,
    Option<Result<(), String>>,
    String,
);

/// The three sentences a click on the notice can end in.
fn sentences() -> Vec<Sentence> {
    vec![
        (
            "the page opened",
            DEFAULT_SOURCE,
            Some(Ok(())),
            format!("Opened {DEFAULT_PAGE}. {HOW}"),
        ),
        (
            "the source has no page",
            MIRROR,
            None,
            format!("v{} is available. {HOW}", next(1)),
        ),
        (
            "the opener failed",
            DEFAULT_SOURCE,
            Some(Err(NO_OPENER.to_string())),
            format!("could not open the link: {NO_OPENER}. {HOW}"),
        ),
    ]
}

/// A click on the notice of `app` drawn at `w` by `h`, and the opener's answer when the click handed it a page.
fn click_and_answer(app: &mut App, w: u16, h: u16, answer: Option<Result<(), String>>) {
    let effects = click_notice(app, w, h);
    match answer {
        None => assert_eq!(effects, vec![], "a source without a page opens nothing"),
        Some(result) => {
            let [Effect::OpenUpdatePage { url, .. }] = effects.as_slice() else {
                panic!("{effects:?}")
            };
            let answered = app.update(Msg::UpdatePage {
                url: url.clone(),
                result,
            });
            assert_eq!(answered, vec![]);
        }
    }
}

/// The screen of [`screens`] named `name`.
fn screen_named(name: &str) -> App {
    screens()
        .into_iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, app, _)| app)
        .unwrap()
}

/// The screens a person is on most of the time.
const MAIN_SCREENS: [&str; 5] = [
    "runs",
    "new run",
    "run, browsing",
    "run, typing",
    "settings",
];

/// The sentence of a click stands on the last rows of the frame, from the first column, and the screen above it is, cell for cell, the
/// screen a terminal that many rows shorter shows: the sentence hides nothing. Two apps of the same screen are clicked, and only one
/// hears from the opener.
fn assert_rows_of_its_own(name: &str, w: u16, h: u16, head: Option<Head>) {
    let said = format!("Opened {DEFAULT_PAGE}. {HOW}");
    let build = || {
        let mut app = noticed_on(screen_named(name), DEFAULT_SOURCE);
        if let Some(head) = head {
            app.set_head(head);
        }
        app
    };
    let mut quiet = build();
    assert_eq!(click_notice(&mut quiet, w, h).len(), 1, "{name}");
    let mut told = build();
    click_and_answer(&mut told, w, h, Some(Ok(())));
    let with = draw(&told, w, h);
    let last = |n: u16| -> Vec<String> {
        (h - n..h)
            .map(|y| row_text(&with, y).trim_end().to_string())
            .collect()
    };
    let rows = (1..=4u16)
        .find(|n| last(*n).join(" ") == said)
        .unwrap_or_else(|| {
            panic!(
                "{name} at {w}x{h}: the sentence is not the last rows of the screen: {:?}",
                last(4)
            )
        });
    let without = draw(&quiet, w, h - rows);
    for y in 0..h - rows {
        assert_eq!(
            row_text(&with, y),
            row_text(&without, y),
            "{name} at {w}x{h}: row {y} of the screen above the sentence"
        );
    }
}

#[test]
fn b513_the_sentence_of_a_click_is_whole_at_ordinary_widths() {
    let _g = pinned(Some(CLIENT));
    for (what, source, answer, said) in sentences() {
        for (w, h) in [(80u16, 24u16), (100, 30)] {
            // TRIGGER PRESENT: with the default source the sentence is longer than a row of these terminals, and how to update is its end.
            if source == DEFAULT_SOURCE {
                assert!(said.chars().count() > usize::from(w), "{what}: {said}");
            }
            for name in MAIN_SCREENS {
                let mut app = noticed_on(screen_named(name), source);
                click_and_answer(&mut app, w, h, answer.clone());
                let text = screen_text(&app, w, h);
                assert!(
                    text.contains("run ham update"),
                    "{name} at {w}x{h}, {what}: how to update is on the screen: {text}"
                );
                assert!(
                    text.contains(&said),
                    "{name} at {w}x{h}, {what}: the whole sentence is on the screen: {text}"
                );
            }
        }
    }
}

#[test]
fn b513_the_sentence_of_a_click_has_rows_of_its_own_under_the_screen() {
    let _g = pinned(Some(CLIENT));
    for name in MAIN_SCREENS {
        // Two rows at 80 and at 100 columns, one row where the sentence fits.
        for (w, h) in [(80u16, 24u16), (100, 30), (160, 40)] {
            assert_rows_of_its_own(name, w, h, None);
        }
        // With the title band of a tall terminal (five rows from 33 rows).
        assert_rows_of_its_own(name, 120, 44, Some(Head::banded(Look::Blocks)));
    }
    // What a person types is not under the sentence: the draft of the message box stays on the screen.
    let mut app = noticed_on(typing(), DEFAULT_SOURCE);
    for c in "a draft of mine".chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
    click_and_answer(&mut app, 80, 24, Some(Ok(())));
    let text = screen_text(&app, 80, 24);
    assert!(text.contains("a draft of mine"), "{text}");
    assert!(
        text.contains(&format!("Opened {DEFAULT_PAGE}. {HOW}")),
        "{text}"
    );
}

#[test]
fn b513_the_sentence_of_a_click_stays_until_the_next_key_or_press() {
    let _g = pinned(Some(CLIENT));
    let said = format!("v{} is available. {HOW}", next(1));
    let shown = |app: &App| screen_text(app, 120, 30).contains(&said);
    let at = Instant::now();
    // A tick and a move of the mouse leave it.
    let mut app = noticed_on(new_app(), MIRROR);
    click_and_answer(&mut app, 120, 30, None);
    assert!(shown(&app));
    app.tick();
    app.handle_mouse_at(mouse(MouseEventKind::Moved, 40, 10), at);
    assert!(shown(&app), "a move of the mouse leaves the sentence");
    // The next key takes it away and does what it always does.
    app.handle_key(key(KeyCode::Char('s')));
    assert_eq!(app.screen(), Screen::Settings);
    assert!(!shown(&app), "the next key takes the sentence away");
    assert_eq!(notice_on(&app, 120, 30), Some(notice()), "the notice stays");

    // So does the next press of a mouse button, wherever it lands.
    let mut app = noticed_on(dashboard(), MIRROR);
    click_and_answer(&mut app, 120, 30, None);
    assert!(shown(&app));
    app.handle_mouse_at(mouse(MouseEventKind::Down(MouseButton::Left), 40, 10), at);
    assert!(!shown(&app), "the next press takes the sentence away");

    // And a paste, which goes where it always goes.
    let mut app = noticed_on(typing(), MIRROR);
    click_and_answer(&mut app, 120, 30, None);
    assert!(shown(&app));
    app.handle_paste("pasted words".to_string());
    let text = screen_text(&app, 120, 30);
    assert!(!text.contains(&said), "{text}");
    assert!(text.contains("pasted words"), "{text}");
}

#[test]
fn b513_a_long_address_keeps_how_to_update_on_the_screen() {
    let _g = pinned(Some(CLIENT));
    // An update source of about 400 characters: its sentence would need six rows at 80 columns, and it gets four.
    let long = format!(
        "https://git.example.test/{}ham.git",
        "team-of-many-words/".repeat(20)
    );
    assert!(long.len() > 400);
    for (name, app) in [("runs", new_app()), ("run, typing", typing())] {
        let mut app = noticed_on(app, &long);
        click_and_answer(&mut app, 80, 24, Some(Ok(())));
        let buf = draw(&app, 80, 24);
        let last: Vec<String> = (20..24u16)
            .map(|y| row_text(&buf, y).trim_end().to_string())
            .collect();
        // An address longer than a row starts on a row of its own and is split where the row ends.
        assert_eq!(last[0], "Opened", "{name}: {last:?}");
        assert!(
            last[1].starts_with("https://git.example.test/team-of-many-words/")
                && last[1].chars().count() == 80,
            "{name}: {last:?}"
        );
        assert!(
            last[2].ends_with('…') && last[2].chars().count() == 80,
            "{name}: what is cut of the address ends in an ellipsis: {last:?}"
        );
        assert_eq!(
            last[3], HOW,
            "{name}: how to update is whole on the last row: {last:?}"
        );
    }
}

#[test]
fn b513_a_click_says_its_sentence_on_every_screen_and_over_both_overlays() {
    let _g = pinned(Some(CLIENT));
    for (what, source, answer, said) in sentences() {
        for (w, h) in [(80u16, 24u16), (120, 40)] {
            for (name, app, title) in screens() {
                let mut app = noticed_on(app, source);
                let screen = app.screen();
                let before = screen_text(&app, w, h);
                click_and_answer(&mut app, w, h, answer.clone());
                let text = screen_text(&app, w, h);
                assert!(text.contains(&said), "{name} at {w}x{h}, {what}: {text}");
                assert_eq!(app.screen(), screen, "{name}: the app stays where it was");
                // The sentence is no error of the screen it shows on (the Reply screen has a line for an error of the reply).
                assert_eq!(
                    text.contains("error:"),
                    before.contains("error:"),
                    "{name} at {w}x{h}, {what}: {text}"
                );
                // The title stays leftmost; at 80 columns it keeps the 36 columns the notice and the tag leave it.
                let row = row_text(&draw(&app, w, h), 0);
                let kept: String = title.chars().take(35).collect();
                assert!(row.starts_with(&kept), "{name} at {w}x{h}: {row:?}");
                if w >= 100 {
                    assert!(row.starts_with(&title), "{name} at {w}x{h}: {row:?}");
                }
            }
        }
    }
    // On every screen and over both overlays the sentence has rows of its own: the tag explorer and the open-anything box stay open
    // and whole above it, and so do the Reply, Models and Artifacts screens.
    for (name, _, _) in screens() {
        for (w, h) in [(80u16, 24u16), (120, 40)] {
            assert_rows_of_its_own(name, w, h, None);
        }
    }
    let over = |name: &str| {
        let mut app = noticed_on(screen_named(name), MIRROR);
        click_and_answer(&mut app, 100, 30, None);
        draw(&app, 100, 30)
    };
    // The open-anything box keeps its frame: its last row is the row above the sentence.
    let boxed = over("open anything");
    let frame_end = row_text(&boxed, 28);
    assert!(
        frame_end.starts_with('└') && frame_end.trim_end().ends_with('┘'),
        "{frame_end:?}"
    );
    assert!(row_text(&boxed, 0).contains("Open anything"));
    assert_eq!(
        row_text(&boxed, 29).trim_end(),
        format!("v{} is available. {HOW}", next(1))
    );
    // The tag explorer keeps its key line.
    let tags = over("tag explorer");
    assert!(
        row_text(&tags, 28).contains("Esc"),
        "{:?}",
        row_text(&tags, 28)
    );
}

async fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
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

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell
#[tokio::test]
async fn b513_the_stand_in_opener_gets_the_page_as_its_one_argument() {
    let _g = pinned(Some(CLIENT));
    let fake = Fake::start().await;
    let dir = support::scratch("b513-tui-opener");
    let (program, record) = stand_in(&dir, 0);
    let mut app = noticed(new_app());
    app.set_open_command(Some(program.clone()));
    let effects = click_notice(&mut app, 200, 40);
    assert_eq!(effects.len(), 1, "{effects:?}");
    // The event loop performs the effect: the opener runs with the page and nothing else.
    let mut answers = Vec::new();
    for effect in effects {
        answers.push(perform(&api(&fake).await, &dir, effect).await);
    }
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("{PAGE}\n"),
        "the opener gets the web page of the update source as exactly one argument"
    );
    let Some(Msg::UpdatePage { url, result }) = answers.pop().unwrap() else {
        panic!("the opener's answer is the message of the notice")
    };
    assert_eq!((url.as_str(), &result), (PAGE, &Ok(())));
    app.update(Msg::UpdatePage { url, result });
    assert!(screen_text(&app, 200, 40).contains(&format!("Opened {PAGE}. {HOW}")));
    assert_eq!(app.screen(), Screen::Runs, "the app stays open");
}

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell
#[tokio::test]
async fn b513_the_stand_in_opener_gets_a_page_with_a_query_letter_for_letter() {
    let _g = pinned(Some(CLIENT));
    let fake = Fake::start().await;
    let dir = support::scratch("b513-tui-opener-literal");
    let (program, record) = stand_in(&dir, 0);
    // An update source with a query and a fragment that hold ordinary characters a shell would read: `&`, `;`, `$HOME`, `?`, `#`.
    // Such an address is handed over as it is, so the `.git` in front of the query stays.
    let source = "https://git.example.test/team/ham.git?a=1&b=2;second;$HOME;end#top";
    let mut app = noticed_on(new_app(), source);
    app.set_open_command(Some(program.clone()));
    let effects = click_notice(&mut app, 200, 40);
    assert_eq!(
        effects,
        vec![Effect::OpenUpdatePage {
            program,
            url: source.into(),
        }]
    );
    for effect in effects {
        let answer = perform(&api(&fake).await, &dir, effect).await.unwrap();
        assert!(
            matches!(&answer, Msg::UpdatePage { url, result: Ok(()) } if url == source),
            "{answer:?}"
        );
        app.update(answer);
    }
    // One line of the record is one argument: the opener got the page whole, and no shell read a character of it.
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("{source}\n"),
        "the opener gets the page as exactly one argument, letter for letter"
    );
    assert!(screen_text(&app, 200, 40).contains(&format!("Opened {source}. {HOW}")));
}

#[allow(clippy::await_holding_lock)] // the guard serialises tests that share the process-wide cell
#[tokio::test]
async fn b513_a_stand_in_opener_that_fails_is_reported_with_how_to_update() {
    let _g = pinned(Some(CLIENT));
    let fake = Fake::start().await;
    let dir = support::scratch("b513-tui-opener-fails");
    // An opener that is not there, and one that ends with a failure.
    let (failing, record) = stand_in(&dir, 3);
    for (program, names) in [
        (
            "/nonexistent/ham-opener".to_string(),
            "could not run /nonexistent/ham-opener",
        ),
        (failing, "exited with code 3"),
    ] {
        let mut app = noticed(new_app());
        app.set_open_command(Some(program));
        let effects = click_notice(&mut app, 240, 40);
        let [effect] = effects.as_slice() else {
            panic!("{effects:?}")
        };
        let answer = perform(&api(&fake).await, &dir, effect.clone())
            .await
            .unwrap();
        let Msg::UpdatePage { result: Err(e), .. } = &answer else {
            panic!("{answer:?}")
        };
        assert!(e.contains(names), "{e}");
        app.update(answer);
        let text = screen_text(&app, 240, 40);
        assert!(
            text.contains("could not open the link: ") && text.contains(names),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "(set HAMH_OPEN_COMMAND to a program that opens a URL). {HOW}"
            )),
            "{text}"
        );
        assert_eq!(app.screen(), Screen::Runs);
    }
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("{PAGE}\n"),
        "the opener that ran got the page"
    );
}

// ---- when the app looks --------------------------------------------------------------------------------------------------------------

#[test]
fn b513_the_first_effects_hold_one_look() {
    // Before the loop's first draw the app has no clock yet: the first look needs none.
    let mut app = new_app();
    app.set_update_look(Some(PathBuf::from(GIT)));
    assert_eq!(
        app.update_look_first(),
        vec![Effect::LookForUpdate {
            generation: 0,
            program: PathBuf::from(GIT),
        }]
    );
    // One look at a time.
    assert_eq!(app.update_look_first(), vec![]);
    assert_eq!(looks(&app.tick()), vec![]);
    // An app that was told no git starts none.
    let mut app = new_app();
    assert_eq!(app.update_look_first(), vec![]);
    app.set_update_look(None);
    assert_eq!(looks(&app.tick()), vec![]);
}

#[test]
fn b513_tick_starts_looks_by_the_schedule() {
    let _g = pinned(Some(CLIENT));
    const T0: i64 = 1_792_000_000;
    const DAY: i64 = 86_400;
    const HOUR: i64 = 3_600;
    let mut app = new_app();
    app.set_now(T0);
    let (mut app, first) = looking(app);
    let look_at = |app: &mut App, now: i64| {
        app.set_now(now);
        looks(&app.tick())
    };
    // While the first look is in flight no tick starts another, however long it takes.
    assert_eq!(look_at(&mut app, T0 + 1), vec![]);
    assert_eq!(look_at(&mut app, T0 + 2 * DAY), vec![]);
    // It ends with a release at T0: the next look is a day later, not before.
    app.set_now(T0);
    answer(&mut app, first, LookOutcome::Newest(next(1)), Some(SOURCE));
    assert_eq!(look_at(&mut app, T0 + 1), vec![]);
    assert_eq!(look_at(&mut app, T0 + DAY - 1), vec![]);
    let second = look_at(&mut app, T0 + DAY);
    assert_eq!(second, vec![(first, PathBuf::from(GIT))]);
    assert_eq!(
        look_at(&mut app, T0 + DAY + 1),
        vec![],
        "one look at a time"
    );
    // That look fails: it is tried again after an hour, and the notice of the earlier look stays.
    app.set_now(T0 + DAY);
    answer(&mut app, second[0].0, LookOutcome::Failed, Some(SOURCE));
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    assert_eq!(look_at(&mut app, T0 + DAY + HOUR - 1), vec![]);
    let third = look_at(&mut app, T0 + DAY + HOUR);
    assert_eq!(third.len(), 1);
    // The source holds no release any more: the notice goes, and the next look is a day later.
    app.set_now(T0 + DAY + HOUR);
    answer(&mut app, third[0].0, LookOutcome::NoRelease, Some(SOURCE));
    assert_eq!(notice_on(&app, 120, 30), None);
    assert_eq!(look_at(&mut app, T0 + DAY + HOUR + DAY - 1), vec![]);
    assert_eq!(look_at(&mut app, T0 + DAY + HOUR + DAY).len(), 1);
}

/// An environment with a config folder of its own that holds `config`.
fn env_with(label: &str, config: &str) -> impl Fn(&str) -> Option<String> + use<> {
    let root = support::scratch(label);
    let dir = root.join("config/ham");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    let home = root.join("config").display().to_string();
    move |name| (name == "XDG_CONFIG_HOME").then(|| home.clone())
}

const CONFIG: &str = "configVersion = 1\napiEndpoint = \"https://example.execute-api.us-east-1.amazonaws.com/direct\"\nregion = \"us-east-1\"\n";

#[test]
fn b513_a_settings_write_clears_the_notice_and_looks_again() {
    let _g = pinned(Some(CLIENT));
    let env = env_with("b513-tui-settings", CONFIG);
    let write = |key: Key, value: &str| {
        Msg::Settings(settings::perform(
            &env,
            Job::Apply {
                key,
                op: Op::Set(value.to_string()),
            },
        ))
    };
    let other = "https://git.example.test/other/ham.git";

    // No look in flight: a write of the source takes the notice away at once and starts a look of the next generation.
    let mut app = noticed(new_app());
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    let started = looks(&app.update(write(Key::UpdateSource, other)));
    assert_eq!(started, vec![(1, PathBuf::from(GIT))]);
    assert_eq!(notice_on(&app, 120, 30), None, "the notice goes at once");
    // The look of the new settings names the release on the new source: the click opens the new page.
    answer(&mut app, 1, LookOutcome::Newest(next(1)), Some(other));
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    assert_eq!(
        click_notice(&mut app, 160, 30),
        vec![Effect::OpenUpdatePage {
            program: OPENER.into(),
            url: "https://git.example.test/other/ham".into(),
        }]
    );

    // A look in flight: the new look starts when that one has ended, and the result of the earlier settings is dropped.
    let (mut app, first) = looking(new_app());
    assert_eq!(
        looks(&app.update(write(Key::UiUpdateNotice, "off"))),
        vec![]
    );
    assert_eq!(
        answer(&mut app, first, LookOutcome::Newest(next(1)), Some(SOURCE)),
        vec![]
    );
    assert_eq!(
        notice_on(&app, 120, 30),
        None,
        "a look of earlier settings raises no notice"
    );
    let again = looks(&app.tick());
    assert_eq!(again, vec![(first + 1, PathBuf::from(GIT))]);
    // The look finds the notice switched off: it answers Off, and there is no notice.
    answer(&mut app, again[0].0, LookOutcome::Off, None);
    assert_eq!(notice_on(&app, 120, 30), None);
    assert_eq!(looks(&app.tick()), vec![]);

    // A write of another key does neither.
    let mut app = noticed(new_app());
    assert_eq!(looks(&app.update(write(Key::UiBell, "off"))), vec![]);
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
}

#[test]
fn b513_a_look_that_could_not_read_the_config_keeps_the_notice_and_its_page() {
    let _g = pinned(Some(CLIENT));
    const T0: i64 = 1_792_000_000;
    let mut app = new_app();
    app.set_now(T0);
    let mut app = noticed(app);
    // The look of the next day finds a config it cannot read (an editor has the file open): it answers as a look that failed, without
    // a source.
    app.set_now(T0 + 86_400);
    let second = looks(&app.tick());
    assert_eq!(second.len(), 1);
    assert_eq!(
        answer(&mut app, second[0].0, LookOutcome::Failed, None),
        vec![]
    );
    // The notice of the earlier look stays, and a click still opens the page that look read.
    assert_eq!(notice_on(&app, 120, 30), Some(notice()));
    assert_eq!(
        click_notice(&mut app, 160, 30),
        vec![Effect::OpenUpdatePage {
            program: OPENER.into(),
            url: PAGE.into(),
        }]
    );
    // The look is tried again after an hour, not after a day.
    app.set_now(T0 + 86_400 + 3_599);
    assert_eq!(looks(&app.tick()), vec![]);
    app.set_now(T0 + 86_400 + 3_600);
    assert_eq!(looks(&app.tick()).len(), 1);
}

// ---- what the notice costs a draw ----------------------------------------------------------------------------------------------------

/// `n` agents of the run, each with a session, a turn and a tool call: the screen whose draw `tui_w2` holds to its bound.
fn many_agents(n: u32) -> Vec<Envelope> {
    let event = |k: u32, t: EventType, agent: Option<&str>, v: Value| Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A000000000000000{k:06}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: 0,
        ts: "2026-09-27T09:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    };
    let mut events = vec![event(
        0,
        EventType::RunCreated,
        None,
        json!({"target": {"mode": "s3-only"}}),
    )];
    let mut k = 1;
    for i in 0..n {
        let a = format!("A{i:04}");
        events.push(event(
            k,
            EventType::SessionStarted,
            None,
            json!({"agentId": a, "engine": "cline", "modelId": "us.anthropic.claude-sonnet-5"}),
        ));
        events.push(event(
            k + 1,
            EventType::TurnStarted,
            Some(&a),
            json!({"turn": 1}),
        ));
        events.push(event(
            k + 2,
            EventType::ToolSummary,
            Some(&a),
            json!({"toolCallId": format!("call-{i}"), "title": format!("editing src/module_{i}.py"), "kind": "edit", "status": "pending"}),
        ));
        k += 3;
    }
    events
}

/// The time this thread has spent ready to run and waiting for a processor: the second number of `/proc/thread-self/schedstat`
/// (Linux). `None` where the machine has no such file.
fn waited_for_a_processor() -> Option<Duration> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let nanos = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

/// How long one draw of `app` at 120 by 40 takes, read as the draw bound of `tui_w2` reads its time: the wall clock less the time the
/// drawing thread waited for a processor, where the machine reports that wait.
fn draw_time(app: &App) -> Duration {
    let (wall, waited) = (Instant::now(), waited_for_a_processor());
    draw(app, 120, 40);
    let (waited_now, took) = (waited_for_a_processor(), wall.elapsed());
    match (waited, waited_now) {
        (Some(before), Some(after)) => took.saturating_sub(after.saturating_sub(before)),
        _ => took,
    }
}

/// The bound of what the notice costs a draw, held with every build. Every draw of every screen plans the notice (the client's
/// version, the backend's and the newest release the last look named) and draws its words on the first row. The same run screen with
/// 5,000 agents is drawn without a notice and with one, in turn, 40 times each, and the quickest draw with the notice may take a
/// quarter and a millisecond longer than the quickest draw without it. The quickest of many draws leaves out what other work on the
/// machine adds to a single one.
#[test]
fn b513_a_draw_with_the_notice_takes_no_longer_than_a_draw_without_it() {
    let _g = pinned(Some(CLIENT));
    let run = || {
        let mut app = new_app();
        app.open_run(RUN);
        app.update(pages(many_agents(5_000)));
        app
    };
    let plain = run();
    let with = noticed(run());
    // TRIGGER PRESENT: both draw the 5,000 agents, and only one of them shows the notice.
    assert_eq!(notice_on(&plain, 120, 40), None);
    assert_eq!(notice_on(&with, 120, 40), Some(notice()));
    for app in [&plain, &with] {
        let text = screen_text(app, 120, 40);
        assert!(text.contains("agents 5000"), "{text}");
    }
    let (mut without_it, mut with_it) = (Duration::MAX, Duration::MAX);
    for _ in 0..40 {
        without_it = without_it.min(draw_time(&plain));
        with_it = with_it.min(draw_time(&with));
    }
    let allowed = without_it + without_it / 4 + Duration::from_millis(1);
    eprintln!(
        "b513 draw of 5,000 agents at 120x40, the quickest of 40: {without_it:?} without the notice, {with_it:?} with it (allowed {allowed:?})"
    );
    assert!(
        with_it <= allowed,
        "a draw with the notice took {with_it:?}; without it {without_it:?}; allowed {allowed:?}"
    );
}
