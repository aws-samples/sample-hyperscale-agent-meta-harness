//! The first row of the Runs screen is drawn after ONE list call: the event loop lays the newest page of a whole read while the second
//! page is still unanswered (`ham::tui::load_runs` sends `Msg::RunsNewest`, then `Msg::RunsOlder`). The order is held by a plane that
//! keeps its second answer until the test has seen the first row; no time is asserted (the 10 s below are a guard that ends a failing
//! run with the frames it saw).
#![allow(clippy::unwrap_used)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::{App, DirectSettings, Msg, load_runs};
use ham_core::runfilter::RunsRead;
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ARN: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const LOADING: &str = " runs \u{b7} loading older runs ";

fn id(n: usize) -> String {
    format!("01M5A0000000000000{n:08}")
}

fn row(n: usize) -> Value {
    let back = 43_200 - n;
    json!({"runId": id(n), "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": format!("2026-10-20T{:02}:{:02}:{:02}Z", back / 3600, (back / 60) % 60, back % 60),
        "ownerPrincipalArn": ARN})
}

fn reply(status: u16, body: Value) -> Result<Response, TransportError> {
    Ok(Response {
        status,
        headers: BTreeMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
    })
}

/// A plane whose list has 250 runs in pages of 100. A call that carries a page token waits while the gate is closed.
struct Paged {
    closed: AtomicBool,
    /// The paths of the list calls, in the order they arrived.
    calls: Mutex<Vec<String>>,
    answered: AtomicUsize,
    /// The list calls out now, and the most that were out together.
    out: AtomicUsize,
    most_out: AtomicUsize,
}

impl ControlPlane for Paged {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        if path != "/v1/runs" && !path.starts_with("/v1/runs?") {
            return reply(
                404,
                json!({"code": "not-found", "message": "not a route of this plane"}),
            );
        }
        self.calls.lock().unwrap().push(path.to_string());
        let out = self.out.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_out.fetch_max(out, Ordering::SeqCst);
        let (from, to, next) = match path {
            "/v1/runs" => (0, 100, Some("p1")),
            "/v1/runs?nextToken=p1" => (100, 200, Some("p2")),
            "/v1/runs?nextToken=p2" => (200, 250, None),
            _ => (0, 0, None),
        };
        if path != "/v1/runs" {
            while self.closed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let mut body = json!({"runs": (from..to).map(row).collect::<Vec<_>>()});
        if let Some(t) = next {
            body["nextToken"] = json!(t);
        }
        self.out.fetch_sub(1, Ordering::SeqCst);
        self.answered.fetch_add(1, Ordering::SeqCst);
        reply(200, body)
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("these calls fetch nothing")
    }
}

/// A terminal that keeps the text of every frame it was asked to show.
struct Frames {
    inner: TestBackend,
    frames: Arc<Mutex<Vec<String>>>,
}

impl Backend for Frames {
    type Error = std::convert::Infallible;
    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        let buf = self.inner.buffer();
        let area = buf.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        self.frames.lock().unwrap().push(text);
        self.inner.flush()
    }
}

/// The first kept frame that `wanted` accepts, looked for until the guard of 10 s ends the wait.
async fn frame(frames: &Arc<Mutex<Vec<String>>>, wanted: impl Fn(&str) -> bool) -> Option<String> {
    let guard = Instant::now();
    loop {
        if let Some(f) = frames.lock().unwrap().iter().find(|f| wanted(f)) {
            return Some(f.clone());
        }
        if guard.elapsed() > Duration::from_secs(10) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b961_the_first_row_is_drawn_while_the_second_list_call_is_still_unanswered() {
    let plane = Arc::new(Paged {
        closed: AtomicBool::new(true),
        calls: Mutex::default(),
        answered: AtomicUsize::new(0),
        out: AtomicUsize::new(0),
        most_out: AtomicUsize::new(0),
    });
    let api = Api::new(Arc::clone(&plane)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let frames: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut term = Terminal::new(Frames {
        inner: TestBackend::new(120, 30),
        frames: Arc::clone(&frames),
    })
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let first = app.start();
    // The download folder is never written by this test: nothing is downloaded.
    let dir = std::env::temp_dir().join(format!("ham-test-b961-first-row-{}", std::process::id()));
    // The timer is far away: the only list calls are those of the whole read.
    let the_loop = ham::tui::event_loop(
        &mut term,
        api,
        dir,
        app,
        first,
        rx,
        Duration::from_secs(3600),
    );
    let seen = Arc::clone(&frames);
    let held = Arc::clone(&plane);
    let driver = async move {
        // The first row, while the plane holds the answer to the second list call.
        let row = frame(&seen, |f| f.contains(&id(0)) && f.contains(LOADING)).await;
        let answered_then = held.answered.load(Ordering::SeqCst);
        let calls_then = held.calls.lock().unwrap().clone();
        held.closed.store(false, Ordering::SeqCst);
        let whole = frame(&seen, |f| f.contains(" run 1 of 250 ")).await;
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        (row, answered_then, calls_then, whole)
    };
    let (ended, (row, answered_then, calls_then, whole)) = tokio::join!(the_loop, driver);
    ended.unwrap();
    let last = frames.lock().unwrap().last().cloned().unwrap_or_default();
    assert!(
        row.is_some(),
        "no frame showed the newest run while the second list call was unanswered; the last frame:\n{last}"
    );
    assert_eq!(
        answered_then, 1,
        "one list call was answered when the first row was drawn"
    );
    assert_eq!(
        calls_then,
        ["/v1/runs", "/v1/runs?nextToken=p1"],
        "the second call was out and held when the first row was drawn"
    );
    let whole =
        whole.unwrap_or_else(|| panic!("no frame showed the whole list; the last frame:\n{last}"));
    assert!(!whole.contains(LOADING), "{whole}");
    assert_eq!(
        plane.calls.lock().unwrap().clone(),
        ["/v1/runs", "/v1/runs?nextToken=p1", "/v1/runs?nextToken=p2"],
        "three list calls for the read, in the tokens' order"
    );
    assert_eq!(
        plane.most_out.load(Ordering::SeqCst),
        1,
        "never two list calls out together"
    );
}

/// A plane that answers each call with the next scripted reply.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let (status, body) = self.replies.lock().unwrap().pop_front().unwrap_or((
            500,
            json!({"code": "unscripted", "message": "no reply scripted"}),
        ));
        reply(status, body)
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("these calls fetch nothing")
    }
}

/// The messages one whole read sends for these replies.
async fn messages(replies: Vec<(u16, Value)>) -> Vec<Msg> {
    let plane = Arc::new(Scripted::default());
    *plane.replies.lock().unwrap() = replies.into_iter().collect();
    let api = Api::new(plane).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let mut sent: Vec<Msg> = Vec::new();
    load_runs(&api, &mut |m| sent.push(m)).await;
    sent
}

fn ids(read: &RunsRead) -> Vec<String> {
    let runs = match read {
        RunsRead::All(runs) | RunsRead::First { runs, .. } => runs,
    };
    runs.iter().map(|r| r.run_id.clone()).collect()
}

#[tokio::test]
async fn b961_a_whole_read_chooses_its_messages() {
    let page = |from: usize, to: usize, next: Option<&str>| {
        let mut body = json!({"runs": (from..to).map(row).collect::<Vec<_>>()});
        if let Some(t) = next {
            body["nextToken"] = json!(t);
        }
        (200, body)
    };
    // (i) Three pages: the newest page, then the whole list.
    let sent = messages(vec![
        page(0, 2, Some("t2")),
        page(2, 4, Some("t3")),
        page(4, 5, None),
    ])
    .await;
    match sent.as_slice() {
        [Msg::RunsNewest(newest), Msg::RunsOlder(Ok(all))] => {
            assert!(
                matches!(newest.read, RunsRead::First { has_more: true, .. }),
                "{newest:?}"
            );
            assert_eq!(ids(&newest.read), [id(0), id(1)]);
            assert!(matches!(all.read, RunsRead::All(_)), "{all:?}");
            assert_eq!(ids(&all.read), [id(0), id(1), id(2), id(3), id(4)]);
        }
        other => panic!("three pages send the newest page and then the whole list: {other:?}"),
    }
    // (i) With the second page failing: the newest page, then the failure of the older runs.
    let sent = messages(vec![
        page(0, 2, Some("t2")),
        (500, json!({"code": "internal", "message": "boom"})),
    ])
    .await;
    assert!(
        matches!(
            sent.as_slice(),
            [Msg::RunsNewest(_), Msg::RunsOlder(Err(_))]
        ),
        "{sent:?}"
    );
    // (ii) A first page without a next token: the one answer of before, the whole list.
    let sent = messages(vec![page(0, 3, None)]).await;
    match sent.as_slice() {
        [Msg::RunsIndexed(Ok(all))] => {
            assert!(matches!(all.read, RunsRead::All(_)), "{all:?}");
            assert_eq!(ids(&all.read), [id(0), id(1), id(2)]);
        }
        other => panic!("a list of one call sends one message: {other:?}"),
    }
    // (iii) A first page that fails: the one failure of before.
    let sent = messages(vec![(500, json!({"code": "internal", "message": "boom"}))]).await;
    assert!(
        matches!(sent.as_slice(), [Msg::RunsIndexed(Err(_))]),
        "{sent:?}"
    );
}
