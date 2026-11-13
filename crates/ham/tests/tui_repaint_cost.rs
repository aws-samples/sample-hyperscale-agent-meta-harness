//! What a full repaint costs, in bytes written to the terminal: the clear (`ESC [ 2 J`) and every cell of the frame. A repaint is rare
//! (the window has focus again, Ctrl-L, a continue) and an unchanged frame costs next to nothing, which is why the timer's one-second
//! frame needs no repaint of its own. The worst case is a frame whose every cell holds text and whose style changes every eight cells.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::{finished_events, ready};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Memory the backend writes to; the test reads how much went in.
#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Vec<u8>>>);

impl Write for Memory {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A crossterm backend that writes to memory and reports a size of its own, so no terminal is needed.
struct Wire {
    inner: CrosstermBackend<Memory>,
    memory: Memory,
    size: Size,
}

impl Wire {
    fn new(w: u16, h: u16) -> Self {
        let memory = Memory::default();
        Self {
            inner: CrosstermBackend::new(memory.clone()),
            memory,
            size: Size::new(w, h),
        }
    }

    /// The bytes written since the last call.
    fn take(&mut self) -> usize {
        std::mem::take(&mut *self.memory.0.lock().unwrap()).len()
    }
}

impl Backend for Wire {
    type Error = io::Error;
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::new(0, 0))
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// First frame, an unchanged frame, and a repaint of the same frame, in bytes.
fn costs(w: u16, h: u16, draw: &dyn Fn(&mut ratatui::Frame)) -> (usize, usize, usize) {
    let mut term = Terminal::new(Wire::new(w, h)).unwrap();
    term.draw(|f| draw(f)).unwrap();
    let first = term.backend_mut().take();
    term.draw(|f| draw(f)).unwrap();
    let idle = term.backend_mut().take();
    ham::tui::clear_screen(&mut term);
    term.draw(|f| draw(f)).unwrap();
    let repaint = term.backend_mut().take();
    (first, idle, repaint)
}

/// Every cell holds a letter and the colour changes every eight cells.
fn worst_case(f: &mut ratatui::Frame) {
    let area = f.area();
    let lines: Vec<Line> = (0..area.height)
        .map(|row| {
            Line::from(
                (0..area.width)
                    .step_by(8)
                    .map(|col| {
                        let colour = Color::Indexed(u8::try_from((row + col) % 200).unwrap() + 16);
                        let n = usize::from(8.min(area.width - col));
                        Span::styled("w".repeat(n), Style::default().fg(colour))
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

#[test]
fn a_repaint_costs_kilobytes_and_an_unchanged_frame_costs_almost_nothing() {
    for (w, h, limit) in [(100u16, 30u16, 14_000usize), (200, 60, 55_000)] {
        let (first, idle, repaint) = costs(w, h, &worst_case);
        eprintln!(
            "REPAINT-COST worst case {w}x{h}: first frame {first} bytes, unchanged frame {idle} bytes, clear and full repaint {repaint} bytes"
        );
        assert!(idle < 100, "an unchanged frame wrote {idle} bytes");
        assert!(
            (first..=limit).contains(&repaint),
            "{w}x{h}: repaint {repaint} bytes (first frame {first}, limit {limit})"
        );
        assert!(
            repaint >= first,
            "a repaint writes at least the first frame"
        );
    }
}

#[test]
fn a_repaint_of_the_run_screen_costs_what_its_first_frame_cost() {
    let app = ready(finished_events());
    for (w, h) in [(100u16, 30u16), (200, 60)] {
        let (first, idle, repaint) = costs(w, h, &|f| app.draw(f));
        eprintln!(
            "REPAINT-COST run screen {w}x{h}: first frame {first} bytes, unchanged frame {idle} bytes, clear and full repaint {repaint} bytes"
        );
        assert!(idle < 100, "an unchanged frame wrote {idle} bytes");
        // The clear is a handful of bytes more than the first frame, which also moved the cursor into place.
        assert!(
            repaint >= first.saturating_sub(64) && repaint <= first + 64,
            "{w}x{h}: repaint {repaint} bytes against a first frame of {first}"
        );
    }
}
