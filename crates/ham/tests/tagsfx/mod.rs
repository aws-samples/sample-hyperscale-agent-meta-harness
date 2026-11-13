//! Shared set-up of the tag explorer and `/tag` tests: the held runs of the Runs screen with tags, the 212-run fixture of the
//! design drawings, keys and the screen as text, and a scripted transport that records every request. Each test file that uses it declares
//! `mod tagsfx;`.
#![allow(dead_code, clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::RunSummary;
use harness_protocol::common::parse_utc_unix;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
/// The moment the design drawings were made: the title reads `loaded at 14:05` and the ages count from it.
pub const NOW: &str = "2026-10-21T14:05:00Z";

/// A run id of 26 characters, `n` in its last digits.
pub fn id(n: usize) -> String {
    format!("01M5A0000000000000{n:08}")
}

/// A run in the stage: `created` is an RFC 3339 time, `target` the JSON of its target.
pub fn summary(run: &str, created: &str, target: Value) -> RunSummary {
    serde_json::from_value(
        json!({"runId": run, "status": "complete", "phase": "delivering", "target": target,
        "createdAt": created, "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

pub fn s3_only(run: &str, created: &str) -> RunSummary {
    summary(run, created, json!({"mode": "s3-only"}))
}

pub fn repo(run: &str, created: &str, name: &str) -> RunSummary {
    summary(
        run,
        created,
        json!({"mode": "repo", "repo": format!("cc:{name}")}),
    )
}

/// An index entry with a list of tags (`Some`, even when empty) or none (`None`: a backend that sends no `tags` member).
pub fn entry(run: &str, tags: Option<&[&str]>) -> IndexEntry {
    IndexEntry {
        run_id: run.into(),
        title: Some(format!("Prompt of {run}")),
        model: None,
        name: None,
        name_source: None,
        tags: tags.map(|l| l.iter().map(|t| (*t).to_string()).collect()),
    }
}

/// The same with a run name (a model's), so the run screen has a name row.
pub fn named(run: &str, name: &str, tags: Option<&[&str]>) -> IndexEntry {
    IndexEntry {
        name: Some(name.into()),
        name_source: Some("ai".into()),
        ..entry(run, tags)
    }
}

pub fn unix(ts: &str) -> i64 {
    parse_utc_unix(ts).unwrap()
}

/// A Runs screen that has read every page of `runs` with `index`, at `NOW`.
pub fn app_with(runs: Vec<RunSummary>, index: Vec<IndexEntry>) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_now(unix(NOW));
    read_all(&mut app, runs, index);
    assert_eq!(app.screen(), Screen::Runs);
    app
}

/// A read of every page.
pub fn read_all(app: &mut App, runs: Vec<RunSummary>, index: Vec<IndexEntry>) {
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(runs),
        index,
    })));
}

/// A read of the newest page; `has_more` says the server sent a next token.
pub fn read_first(app: &mut App, runs: Vec<RunSummary>, has_more: bool, index: Vec<IndexEntry>) {
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::First { runs, has_more },
        index,
    })));
}

/// The twelve runs of the design drawings (newest first) and their tags: `project:billing-export` on all twelve, and the others as the
/// drawing counts them in the picked result (python 11, dynamodb 7, csv-export 5, lambda 4, csv 3, nightly-export 3, s3 2, backfill 1,
/// tax-rounding 1). The ages of `NOW` are the drawing's: 2d, 3d, 9d and (the drawing says `1w`, which `age_text` writes as `2w`) 2w.
pub fn billing() -> Vec<(String, &'static str, Vec<&'static str>)> {
    const P: &str = "project:billing-export";
    vec![
        (
            id(1),
            "2026-10-19T13:00:00Z",
            vec![
                P,
                "tech:python",
                "tech:dynamodb",
                "topic:csv-export",
                "tech:csv",
                "topic:backfill",
            ],
        ),
        (
            id(2),
            "2026-10-19T12:00:00Z",
            vec![
                P,
                "tech:python",
                "tech:dynamodb",
                "topic:csv-export",
                "tech:csv",
            ],
        ),
        (
            id(3),
            "2026-10-18T12:00:00Z",
            vec![P, "tech:python", "tech:aws-lambda", "topic:nightly-export"],
        ),
        (
            id(4),
            "2026-10-17T12:00:00Z",
            vec![
                P,
                "tech:python",
                "tech:aws-lambda",
                "topic:nightly-export",
                "topic:csv-export",
            ],
        ),
        (
            id(5),
            "2026-10-16T12:00:00Z",
            vec![P, "tech:python", "tech:dynamodb", "topic:csv-export"],
        ),
        (
            id(6),
            "2026-10-15T12:00:00Z",
            vec![
                P,
                "tech:python",
                "tech:dynamodb",
                "topic:nightly-export",
                "topic:csv-export",
            ],
        ),
        (
            id(7),
            "2026-10-12T12:00:00Z",
            vec![P, "tech:python", "topic:tax-rounding"],
        ),
        (
            id(8),
            "2026-10-11T12:00:00Z",
            vec![P, "tech:python", "tech:dynamodb", "tech:aws-lambda"],
        ),
        (
            id(9),
            "2026-10-10T12:00:00Z",
            vec![
                P,
                "tech:python",
                "tech:dynamodb",
                "tech:aws-lambda",
                "tech:csv",
            ],
        ),
        (
            id(10),
            "2026-10-06T12:00:00Z",
            vec![P, "tech:python", "tech:s3"],
        ),
        (
            id(11),
            "2026-10-05T12:00:00Z",
            vec![P, "tech:python", "tech:s3"],
        ),
        (id(12), "2026-10-01T12:00:00Z", vec![P, "tech:dynamodb"]),
    ]
}

/// The 212-run Runs screen of the design drawings: the twelve billing runs and 200 older runs that have no tag.
pub fn design() -> App {
    let mut runs = Vec::new();
    let mut index = Vec::new();
    for (run, created, tags) in billing() {
        runs.push(s3_only(&run, created));
        index.push(entry(&run, Some(tags.as_slice())));
    }
    for n in 0..200 {
        let run = id(100 + n);
        runs.push(s3_only(&run, "2026-09-01T12:00:00Z"));
        index.push(entry(&run, Some(&[])));
    }
    app_with(runs, index)
}

/// Four runs, newest first: A holds `tech:rust` and `topic:alpha`, B `topic:alpha`, C `tech:rust` and `topic:beta`, D nothing.
pub fn small() -> (App, [String; 4]) {
    let ids = [id(1), id(2), id(3), id(4)];
    let runs = vec![
        s3_only(&ids[0], "2026-10-20T12:03:00Z"),
        s3_only(&ids[1], "2026-10-20T12:02:00Z"),
        s3_only(&ids[2], "2026-10-20T12:01:00Z"),
        s3_only(&ids[3], "2026-10-20T12:00:00Z"),
    ];
    let index = vec![
        entry(&ids[0], Some(&["tech:rust", "topic:alpha"])),
        entry(&ids[1], Some(&["topic:alpha"])),
        entry(&ids[2], Some(&["tech:rust", "topic:beta"])),
        entry(&ids[3], Some(&[])),
    ];
    (app_with(runs, index), ids)
}

pub fn pair(tag: &str, runs: usize) -> (String, usize) {
    (tag.to_string(), runs)
}

// ---- keys and the screen ----------------------------------------------------------------------------------------------------------

pub fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub fn ch(app: &mut App, c: char) -> Vec<Effect> {
    key(app, KeyCode::Char(c))
}

pub fn type_in(app: &mut App, text: &str) {
    for c in text.chars() {
        let effects = ch(app, c);
        assert!(effects.is_empty(), "{c:?} caused {effects:?}");
    }
}

/// Every line of the screen as the terminal shows it (wide characters whole), the version tag of row 0 left off (it holds the release
/// version), trailing blanks cut.
pub fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(if y == 0 {
            ham::tui::version_tag::split_tag(line.trim_end()).0
        } else {
            line.trim_end()
        });
        out.push('\n');
    }
    out
}

/// The whole of row 0 with the version tag, at `w` columns.
pub fn row_zero(app: &App, w: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut line = String::new();
    let mut x = 0;
    while x < w {
        let sym = buf[(x, 0)].symbol();
        line.push_str(sym);
        x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
    }
    line.trim_end().to_string()
}

pub fn screen(app: &App) -> String {
    screen_text(app, 100, 30)
}

/// One tag row of the overlay as drawn: the cursor mark, the pick mark, the tag and its run count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagRow {
    pub cursor: bool,
    pub picked: bool,
    pub tag: String,
    pub runs: usize,
}

/// The tag rows of the overlay, in order: the lines inside the box whose third column on holds a `kind:value` tag and a count.
pub fn rows(text: &str) -> Vec<TagRow> {
    text.lines()
        .filter_map(|l| {
            let inner: Vec<char> = l.strip_prefix('│')?.chars().collect();
            if inner.len() < 4 {
                return None;
            }
            let body: String = inner[3..].iter().collect();
            let mut words = body.split_whitespace();
            let tag = words.next()?;
            let runs: usize = words.next()?.parse().ok()?;
            tag.contains(':').then(|| TagRow {
                cursor: inner[0] == '▶',
                picked: inner[1] == '+',
                tag: tag.to_string(),
                runs,
            })
        })
        .collect()
}

/// The tags and counts of the rows, in order.
pub fn table(text: &str) -> Vec<(String, usize)> {
    rows(text).into_iter().map(|r| (r.tag, r.runs)).collect()
}

/// The tag under the cursor (the row marked `▶`).
pub fn cursor_tag(text: &str) -> Option<String> {
    rows(text).into_iter().find(|r| r.cursor).map(|r| r.tag)
}

/// The picked tags (the rows marked `+`).
pub fn picked(text: &str) -> Vec<String> {
    rows(text)
        .into_iter()
        .filter(|r| r.picked)
        .map(|r| r.tag)
        .collect()
}

/// The two rows under the box: the note and the help line (the last two rows of the screen).
pub fn bottom_two(text: &str) -> (String, String) {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    (lines[n - 2].to_string(), lines[n - 1].to_string())
}

/// The ids among `ids` that the screen shows, in order.
pub fn listed(text: &str, ids: &[String]) -> Vec<String> {
    ids.iter()
        .filter(|i| text.contains(i.as_str()))
        .cloned()
        .collect()
}

// ---- the scripted transport -------------------------------------------------------------------------------------------------------

/// A control plane that records every request (method, path, body) and answers from a script; with nothing scripted it answers 500.
#[derive(Default)]
pub struct Scripted {
    pub calls: Mutex<Vec<(String, String, Value)>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl Scripted {
    pub fn push(&self, status: u16, body: Value) {
        self.replies.lock().unwrap().push_back((status, body));
    }

    pub fn requests(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path.to_string(), parsed));
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((500, json!({"code": "unscripted", "message": path})));
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
        unreachable!("a tag edit fetches nothing")
    }
}

pub fn api() -> (Api<Scripted>, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    (api, plane)
}

/// The answer of the edit route to a request whose runs all took the edit: `ok` for each, with the tags `tags_of(run id)` gives.
pub fn all_ok(request: &Value, tags_of: impl Fn(&str) -> Vec<String>) -> Value {
    let results: Vec<Value> = request["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let run = e["runId"].as_str().unwrap();
            json!({"runId": run, "status": "ok", "tags": tags_of(run)})
        })
        .collect();
    json!({ "results": results })
}

/// Performs the edit effects among `effects` against the transport the app would use and gives each answer back to the app. Returns the
/// number of effects performed.
pub async fn perform_edits(app: &mut App, api: &Api<Scripted>, effects: &[Effect]) -> usize {
    let mut n = 0;
    for e in effects {
        if matches!(e, Effect::EditTags { .. }) {
            let msg = perform(api, &std::env::temp_dir(), e.clone())
                .await
                .expect("an edit gives a message");
            assert!(matches!(msg, Msg::TagsEdited { .. }), "{msg:?}");
            app.update(msg);
            n += 1;
        }
    }
    n
}
