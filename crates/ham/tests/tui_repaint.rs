//! A terminal that lost part of its picture is repainted (bug "some of the UI elements in the TUI don't re-render correctly after switching apps").
//!
//! ratatui writes only the cells that changed since the last frame. When a window is covered and uncovered, the terminal can lose cells
//! that `ham` still believes are on the screen, and nothing `ham` draws afterwards touches them. The event loop therefore asks for a
//! full repaint (the screen is cleared and the next draw writes every cell) on the events that say the picture may be damaged: the
//! terminal reports that the window has focus again, and the person presses Ctrl-L. It does not repaint on anything else, so a
//! window that loses focus, a key and the one-second timer still cost only the cells that changed.
//!
//! The terminal here is a `TestBackend` shared with the test (`Surface`), so the test can damage "the glass" between two frames the way a
//! compositor does, behind the back of the program.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

/// A terminal whose picture the test can read and damage while the event loop runs.
#[derive(Clone)]
struct Surface {
    glass: Arc<Mutex<TestBackend>>,
    /// Frames drawn (one per `Terminal::draw`, counted at the flush).
    frames: Arc<AtomicUsize>,
    /// Times the whole screen was cleared (`ESC [ 2 J` on a real terminal).
    clears: Arc<AtomicUsize>,
}

impl Surface {
    fn new(w: u16, h: u16) -> Self {
        Self {
            glass: Arc::new(Mutex::new(TestBackend::new(w, h))),
            frames: Arc::default(),
            clears: Arc::default(),
        }
    }

    fn picture(&self) -> Buffer {
        self.glass.lock().unwrap().buffer().clone()
    }

    /// The terminal loses these rows: every cell on them is blank again, as when another window was drawn over them.
    fn lose_rows(&self, rows: &[u16]) {
        let mut glass = self.glass.lock().unwrap();
        let width = glass.buffer().area.width;
        let blank = Cell::default();
        let cells: Vec<(u16, u16, &Cell)> = rows
            .iter()
            .flat_map(|&y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| (x, y, &blank))
            .collect();
        glass.draw(cells.into_iter()).unwrap();
    }

    fn frames(&self) -> usize {
        self.frames.load(Ordering::SeqCst)
    }

    fn clears(&self) -> usize {
        self.clears.load(Ordering::SeqCst)
    }
}

impl Backend for Surface {
    type Error = std::convert::Infallible;
    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.glass.lock().unwrap().draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.glass.lock().unwrap().hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.glass.lock().unwrap().show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.glass.lock().unwrap().get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.glass.lock().unwrap().set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.clears.fetch_add(1, Ordering::SeqCst);
        self.glass.lock().unwrap().clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        if clear_type == ClearType::All {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
        self.glass.lock().unwrap().clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.glass.lock().unwrap().size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.glass.lock().unwrap().window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.frames.fetch_add(1, Ordering::SeqCst);
        self.glass.lock().unwrap().flush()
    }
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    })
}

fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
}

fn ctrl_l() -> Event {
    key(KeyCode::Char('l'), KeyModifiers::CONTROL)
}

const WAIT: Duration = Duration::from_secs(20);

/// Waits until `ok` holds; the failure names what never happened.
async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < WAIT, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for `n` more frames than there are now: the loop has handled what was sent before and drawn at least `n` times since.
async fn frames_after(surface: &Surface, n: usize) {
    let target = surface.frames() + n;
    until(&format!("{n} more frames"), || surface.frames() >= target).await;
}

/// The rows that show something. The title row is first among them.
fn content_rows(b: &Buffer) -> Vec<u16> {
    (0..b.area.height)
        .filter(|&y| (0..b.area.width).any(|x| b[(x, y)].symbol() != " "))
        .collect()
}

/// The screen once it has stopped changing: the first answer of the stand-in stage has put the backend version into the title row.
async fn settled(surface: &Surface) -> Buffer {
    until("the first frame with the backend version", || {
        let b = surface.picture();
        (0..b.area.width)
            .map(|x| b[(x, 0)].symbol().to_string())
            .collect::<String>()
            .contains(&format!("backend v{}", ham::version::CLIENT))
    })
    .await;
    // A few frames at the timer's pace without a change: the Runs list has been read.
    loop {
        let a = surface.picture();
        frames_after(surface, 3).await;
        if surface.picture() == a {
            return a;
        }
    }
}

/// Runs the event loop on a fresh `Surface` of `w` x `h` while `drive` plays the person and the terminal; the loop ends on the `q` that
/// `drive` sends last. The timer is 20 ms, so frames keep coming and a missing repaint cannot hide behind a quiet loop.
async fn with_loop<F, Fut>(w: u16, h: u16, drive: F)
where
    F: FnOnce(Surface, tokio::sync::mpsc::UnboundedSender<Event>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let fake = Fake::start().await;
    let api = api(&fake);
    let surface = Surface::new(w, h);
    let mut term = Terminal::new(surface.clone()).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let app = App::new_direct("n".into(), None, DirectSettings::default());
    let dir = support::scratch("tui-repaint");
    let driver = drive(surface, tx);
    // A test that cannot end the loop fails here instead of hanging.
    let ended = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(
            ham::tui::event_loop(
                &mut term,
                api,
                dir,
                app,
                vec![],
                rx,
                Duration::from_millis(20)
            ),
            driver
        )
    })
    .await;
    let (result, ()) = ended.expect("the loop and the driver did not finish in 90 s");
    result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_screen_is_repainted_when_the_terminal_says_the_window_has_focus_again() {
    with_loop(100, 30, |surface, tx| async move {
        let before = settled(&surface).await;
        let rows = content_rows(&before);
        assert!(rows.len() >= 2, "the Runs screen shows text: {rows:?}");

        // The trigger: the glass lost the rows, and the program does not know.
        surface.lose_rows(&rows);
        assert_ne!(
            surface.picture(),
            before,
            "TRIGGER PRESENT: the damage must be visible"
        );

        // Losing focus, a key and the timer change nothing on the glass: only the cells that changed are written.
        tx.send(Event::FocusLost).unwrap();
        tx.send(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        frames_after(&surface, 5).await;
        assert_ne!(
            surface.picture(),
            before,
            "focus lost, a key and the timer must not repaint the whole screen"
        );
        let clears = surface.clears();

        // Focus gained: one clear, and every cell is written again, so the glass is as it was.
        tx.send(Event::FocusGained).unwrap();
        until("the glass equals the picture before the damage", || {
            surface.picture() == before
        })
        .await;
        assert_eq!(
            surface.clears(),
            clears + 1,
            "one full clear for one focus-gained"
        );

        tx.send(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_l_repaints_a_damaged_screen_and_nothing_else_does() {
    with_loop(100, 30, |surface, tx| async move {
        let before = settled(&surface).await;
        let rows = content_rows(&before);

        surface.lose_rows(&rows);
        assert_ne!(
            surface.picture(),
            before,
            "TRIGGER PRESENT: the damage must be visible"
        );
        let clears = surface.clears();

        // Keys that are not Ctrl-L (a letter, Ctrl-K, Esc, the arrows) leave the glass as it is.
        for e in [
            key(KeyCode::Char('l'), KeyModifiers::NONE),
            key(KeyCode::Char('k'), KeyModifiers::CONTROL),
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Down, KeyModifiers::NONE),
        ] {
            tx.send(e).unwrap();
        }
        frames_after(&surface, 5).await;
        assert_ne!(surface.picture(), before, "no other key repaints");
        assert_eq!(surface.clears(), clears, "no other key clears the screen");

        tx.send(ctrl_l()).unwrap();
        until("the glass equals the picture before the damage", || {
            surface.picture() == before
        })
        .await;
        assert_eq!(surface.clears(), clears + 1, "one clear for one Ctrl-L");

        tx.send(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_l_repaints_the_composer_too() {
    with_loop(100, 30, |surface, tx| async move {
        // `n` opens the composer (new run); the Runs screen was settled before.
        settled(&surface).await;
        tx.send(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        until("the composer is on the glass", || {
            let b = surface.picture();
            (0..b.area.width)
                .map(|x| b[(x, 0)].symbol().to_string())
                .collect::<String>()
                .contains("New run")
        })
        .await;
        frames_after(&surface, 5).await;
        let before = surface.picture();
        let rows = content_rows(&before);

        surface.lose_rows(&rows);
        assert_ne!(
            surface.picture(),
            before,
            "TRIGGER PRESENT: the damage must be visible"
        );
        tx.send(ctrl_l()).unwrap();
        until("the composer is repainted", || surface.picture() == before).await;

        // Esc leaves the composer so that `q` is a command again.
        tx.send(key(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        tx.send(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_focus_events_costs_one_clear() {
    with_loop(100, 30, |surface, tx| async move {
        let before = settled(&surface).await;
        let rows = content_rows(&before);
        surface.lose_rows(&rows);
        let clears = surface.clears();
        // A window that flickers in and out of focus: the events arrive together and are handled before the next draw.
        for _ in 0..50 {
            tx.send(Event::FocusLost).unwrap();
            tx.send(Event::FocusGained).unwrap();
        }
        until("the glass equals the picture before the damage", || {
            surface.picture() == before
        })
        .await;
        frames_after(&surface, 5).await;
        let spent = surface.clears() - clears;
        assert!(
            (1..=3).contains(&spent),
            "100 focus events must not clear the screen once each: {spent} clears"
        );
        tx.send(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
    })
    .await;
}
