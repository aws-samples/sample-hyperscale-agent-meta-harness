//! How the run screen learns the name of its run. A run that is opened without a model's name reads its row once at open, and then
//! every fifth tick while the name is still a seed and the run is under 60 seconds old; the read stops at the first model name. The read
//! is `GET /v1/runs?q=<run id>&since=<10 minutes ago>`, one page. A run that is held with a model's name asks nothing. A reply for another
//! run changes nothing, and a read that fails keeps the name on screen and the schedule. The transport is a scripted double that records
//! every path, so the tests count requests; the clock is the app's own tick counter (and `set_now` for the age taken from `createdAt`).
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::{App, DirectSettings, Effect, Msg, perform};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use harness_protocol::common::parse_utc_unix;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const RUN: &str = "01M5A00000000000000000R001";
const OTHER: &str = "01M5A00000000000000000R002";
const CREATED: &str = "2026-10-20T12:00:00Z";
const SEED: &str = "Fix the login redirect loop";
const AI: &str = "Login redirect loop fix for the OAuth callback";

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl Scripted {
    fn push(&self, status: u16, body: Value) {
        self.replies.lock().unwrap().push_back((status, body));
    }

    fn paths(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push(path.to_string());
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((200, json!({"runs": []})));
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("the name read fetches nothing")
    }
}

fn api() -> (Api<Scripted>, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    (api, plane)
}

fn row(id: &str, name: &str, source: &str) -> Value {
    json!({"runId": id, "status": "active", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": CREATED, "ownerPrincipalArn": ARN, "title": SEED, "name": name, "nameSource": source})
}

fn page(rows: Vec<Value>) -> Value {
    json!({ "runs": rows })
}

fn summary(id: &str) -> RunSummary {
    serde_json::from_value(
        json!({"runId": id, "status": "active", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": CREATED, "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

/// The runs a read was asked for among `effects`.
fn reads(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::ReadRunName { run_id } => Some(run_id.clone()),
            _ => None,
        })
        .collect()
}

/// Performs the name reads among `effects` against the double and gives each answer to the app. Other effects are not the test's business.
async fn perform_reads(app: &mut App, api: &Api<Scripted>, effects: &[Effect]) -> usize {
    let mut n = 0;
    for e in effects {
        if matches!(e, Effect::ReadRunName { .. }) {
            let msg = perform(api, &std::env::temp_dir(), e.clone())
                .await
                .expect("a read gives a message");
            assert!(matches!(msg, Msg::RunName { .. }), "{msg:?}");
            app.update(msg);
            n += 1;
        }
    }
    n
}

fn second_line(app: &App) -> String {
    let (w, h) = (120u16, 30u16);
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    let mut x = 0;
    while x < w {
        let sym = buf[(x, 1)].symbol();
        out.push_str(sym);
        x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
    }
    out.trim_end().to_string()
}

fn name_is_dim(app: &App) -> bool {
    let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer()[(0, 1)]
        .modifier
        .contains(Modifier::DIM)
}

fn listed(app: &mut App, index: Vec<IndexEntry>) {
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![summary(RUN), summary(OTHER)]),
        index,
    })));
}

fn entry(id: &str, name: Option<&str>, source: Option<&str>) -> IndexEntry {
    IndexEntry {
        run_id: id.into(),
        title: Some(SEED.into()),
        model: None,
        name: name.map(str::to_string),
        name_source: source.map(str::to_string),
        tags: None,
    }
}

fn open_from_the_list(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

#[tokio::test]
async fn a_run_with_no_model_name_reads_at_open_then_every_fifth_tick_and_stops_at_the_model_name()
{
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let opened = app.open_run(RUN);
    assert_eq!(reads(&opened), [RUN], "one read at open: {opened:?}");
    plane.push(200, page(vec![row(RUN, SEED, "seed")]));
    assert_eq!(perform_reads(&mut app, &api, &opened).await, 1);
    assert_eq!(second_line(&app), SEED);
    assert!(name_is_dim(&app), "a seed is dim");
    // Four quiet ticks, then the fifth reads; and so on while the name is a seed.
    let mut asked_at = Vec::new();
    for tick in 1..=14 {
        let effects = app.tick();
        let n = reads(&effects).len();
        assert!(n <= 1, "tick {tick}: {effects:?}");
        if n == 1 {
            asked_at.push(tick);
            plane.push(200, page(vec![row(RUN, SEED, "seed")]));
            perform_reads(&mut app, &api, &effects).await;
        }
    }
    assert_eq!(asked_at, [5, 10], "every fifth tick, no sooner");
    // The model's name arrives at the next read; the screen shows it plain, and the reads stop.
    let mut ai_at = None;
    for tick in 15..=20 {
        let effects = app.tick();
        if !reads(&effects).is_empty() {
            plane.push(200, page(vec![row(RUN, AI, "ai")]));
            perform_reads(&mut app, &api, &effects).await;
            ai_at = Some(tick);
        }
    }
    assert_eq!(ai_at, Some(15));
    assert_eq!(second_line(&app), AI);
    assert!(!name_is_dim(&app), "a model's name is plain");
    let before = plane.paths().len();
    for _ in 0..30 {
        assert!(
            reads(&app.tick()).is_empty(),
            "the reads stopped at the model name"
        );
    }
    assert_eq!(plane.paths().len(), before);
}

#[tokio::test]
async fn the_reads_stop_when_the_run_is_60_seconds_old_by_the_ticks_since_it_was_opened() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let opened = app.open_run(RUN);
    let mut total = perform_reads(&mut app, &api, &opened).await;
    let mut late = 0;
    for tick in 1..=75 {
        let effects = app.tick();
        let n = perform_reads(&mut app, &api, &effects).await;
        total += n;
        if tick >= 60 {
            late += n;
        }
    }
    // At open, then at ticks 5, 10 ... 55: the run is 55 seconds old at the last one.
    assert_eq!(total, 12, "{:?}", plane.paths());
    assert_eq!(late, 0);
}

#[tokio::test]
async fn the_age_comes_from_created_at_when_the_list_holds_the_run() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let created = parse_utc_unix(CREATED).unwrap();
    app.set_now(created + 50);
    listed(&mut app, vec![entry(RUN, Some(SEED), Some("seed"))]);
    let opened = open_from_the_list(&mut app);
    assert_eq!(reads(&opened), [RUN]);
    let mut total = perform_reads(&mut app, &api, &opened).await;
    for tick in 1..=30 {
        app.set_now(created + 50 + tick);
        let effects = app.tick();
        total += perform_reads(&mut app, &api, &effects).await;
    }
    // The run is 50 seconds old at open: the read at open, one at tick 5 (55 seconds), none from tick 10 (60 seconds).
    assert_eq!(total, 2, "{:?}", plane.paths());
}

#[tokio::test]
async fn a_run_held_with_a_model_name_asks_nothing_at_open_and_nothing_later() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    listed(&mut app, vec![entry(RUN, Some(AI), Some("ai"))]);
    let opened = open_from_the_list(&mut app);
    assert_eq!(app.focused_run(), Some(RUN));
    assert!(reads(&opened).is_empty(), "{opened:?}");
    for _ in 0..70 {
        assert!(reads(&app.tick()).is_empty());
    }
    assert!(plane.paths().is_empty());
    assert_eq!(second_line(&app), AI);
    assert!(!name_is_dim(&app));
    let _ = api;
}

#[tokio::test]
async fn a_run_held_with_a_seed_name_shows_it_and_reads_once_at_open() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    listed(&mut app, vec![entry(RUN, Some(SEED), Some("seed"))]);
    let opened = open_from_the_list(&mut app);
    assert_eq!(
        second_line(&app),
        SEED,
        "the held name shows before any read"
    );
    assert_eq!(reads(&opened), [RUN]);
    plane.push(200, page(vec![row(RUN, AI, "ai")]));
    perform_reads(&mut app, &api, &opened).await;
    assert_eq!(second_line(&app), AI);
}

#[tokio::test]
async fn a_reply_for_another_run_changes_nothing() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    listed(&mut app, vec![entry(RUN, Some(SEED), Some("seed"))]);
    let opened = open_from_the_list(&mut app);
    plane.push(200, page(vec![row(RUN, SEED, "seed")]));
    perform_reads(&mut app, &api, &opened).await;
    for _ in 0..4 {
        app.tick();
    }
    app.update(Msg::RunName {
        run_id: OTHER.into(),
        name: Some((
            "Some other run entirely named here".into(),
            Some("ai".into()),
        )),
    });
    assert_eq!(second_line(&app), SEED);
    assert!(name_is_dim(&app));
    // The schedule is the one it was: the fifth tick still reads.
    let effects = app.tick();
    assert_eq!(reads(&effects), [RUN]);
}

#[tokio::test]
async fn a_late_seed_never_takes_a_model_s_name_off_the_screen() {
    let (_api, _plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(Msg::RunName {
        run_id: RUN.into(),
        name: Some((AI.into(), Some("ai".into()))),
    });
    app.update(Msg::RunName {
        run_id: RUN.into(),
        name: Some((SEED.into(), Some("seed".into()))),
    });
    assert_eq!(second_line(&app), AI);
    assert!(!name_is_dim(&app));
}

#[tokio::test]
async fn a_reply_for_a_run_that_is_not_on_screen_is_dropped_not_kept_for_later() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    app.update(Msg::RunName {
        run_id: OTHER.into(),
        name: Some((AI.into(), Some("ai".into()))),
    });
    // Opening that run later shows no name until its own read answers.
    let opened = app.open_run(OTHER);
    assert_eq!(reads(&opened), [OTHER], "{opened:?}");
    assert_ne!(second_line(&app), AI);
}

#[tokio::test]
async fn opening_another_run_restarts_the_schedule_of_the_reads() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let first = app.open_run(RUN);
    perform_reads(&mut app, &api, &first).await;
    for _ in 0..3 {
        assert!(reads(&app.tick()).is_empty());
    }
    // Three ticks of the first run are behind: the second run starts counting from its own open.
    let second = app.open_run(OTHER);
    assert_eq!(reads(&second), [OTHER]);
    plane.push(200, page(vec![row(OTHER, SEED, "seed")]));
    perform_reads(&mut app, &api, &second).await;
    for tick in 1..=4 {
        assert!(
            reads(&app.tick()).is_empty(),
            "tick {tick} of the second run"
        );
    }
    assert_eq!(reads(&app.tick()), [OTHER], "the fifth tick reads");
}

#[tokio::test]
async fn the_request_is_one_page_of_the_list_with_the_run_id_and_a_start_ten_minutes_back() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let opened = app.open_run(RUN);
    plane.push(
        200,
        // Two rows and a token: the read takes the row of its run and asks for no second page.
        json!({"runs": [row(OTHER, "Not this one at all here", "ai"), row(RUN, AI, "ai")], "nextToken": "t2"}),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    perform_reads(&mut app, &api, &opened).await;
    let paths = plane.paths();
    assert_eq!(paths.len(), 1, "{paths:?}");
    let path = &paths[0];
    assert!(path.starts_with("/v1/runs?"), "{path}");
    assert!(path.contains(&format!("q={RUN}")), "{path}");
    assert!(!path.contains("nextToken"), "{path}");
    let since = path
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("since="))
        .expect(path)
        .replace("%3A", ":");
    let at = parse_utc_unix(&since).expect(&since);
    assert!(
        (now - 11 * 60..=now - 9 * 60).contains(&at),
        "since {since} is not about ten minutes back from {now}"
    );
    assert_eq!(second_line(&app), AI);
}

#[tokio::test]
async fn a_read_that_fails_or_finds_no_row_keeps_the_name_and_the_schedule() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    listed(&mut app, vec![entry(RUN, Some(SEED), Some("seed"))]);
    let opened = open_from_the_list(&mut app);
    plane.push(503, json!({"code": "unavailable", "message": "retry"}));
    perform_reads(&mut app, &api, &opened).await;
    assert_eq!(second_line(&app), SEED);
    // A page without the run's row (the run is older than the window) is no change either.
    for tick in 1..=10 {
        let effects = app.tick();
        if tick % 5 == 0 {
            assert_eq!(reads(&effects), [RUN], "tick {tick}");
            plane.push(200, page(vec![row(OTHER, AI, "ai")]));
            perform_reads(&mut app, &api, &effects).await;
            assert_eq!(second_line(&app), SEED, "tick {tick}");
        } else {
            assert!(reads(&effects).is_empty(), "tick {tick}");
        }
    }
}

#[tokio::test]
async fn leaving_the_run_ends_its_reads_and_opening_another_starts_its_own() {
    let (api, plane) = api();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    listed(&mut app, vec![entry(RUN, Some(SEED), Some("seed"))]);
    let opened = open_from_the_list(&mut app);
    perform_reads(&mut app, &api, &opened).await;
    // Back to the Runs screen: its timer reads the list, not the name.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    for _ in 0..12 {
        assert!(reads(&app.tick()).is_empty());
    }
    // Another run opened later reads its own row once at open.
    let other = app.open_run(OTHER);
    assert_eq!(reads(&other), [OTHER]);
    let _ = plane;
}
