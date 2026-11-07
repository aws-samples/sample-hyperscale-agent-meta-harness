//! Open anything and the sessions picker. A child module of `app`.
//!
//! One box, opened by `Ctrl-O` or by `/open <words>`, searches what the person has: the runs and repositories in the list already held (at once, with the Runs
//! screen's own word matching) and what was said in the runs (the route `GET /v1/search`, asked once the words have stopped changing for a second). The results are
//! grouped by repository, each row a run with its state, its name (else the title of its prompt) and, for a hit in the conversation, a snippet. Up and Down move the
//! mark, Enter opens the run (at the place of the match for a chat hit: the transcript is read from the hit's `open` token), Esc closes the box. The box is drawn over
//! the whole screen, so nothing below it can be pressed by mistake.

use super::*;
use ham_core::runsearch::{SearchHit, SearchQuery};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, Borders, Clear, Paragraph};

use crate::tui::ext::{Done, Job};
use crate::tui::mouse::Target;

/// The shortest search worth sending.
const MIN_QUERY: usize = 2;
/// Hits asked for.
const HITS: u32 = 20;
/// Seconds the words rest before the search is sent.
const REST: Duration = Duration::from_secs(1);
/// Runs listed when the box has no words yet.
const RECENT: usize = 8;

#[derive(Debug, Default)]
pub(crate) struct OpenBox {
    active: bool,
    query: ChatBox,
    sel: usize,
    hits: Vec<SearchHit>,
    /// The words the hits are for.
    hits_for: String,
    /// The words the last request was sent for.
    asked: String,
    changed: Duration,
    inflight: bool,
    note: Option<String>,
}

/// One row of the results.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    repo: String,
    run_id: String,
    /// What names the run in the list: its name, else the title of its prompt.
    title: String,
    state: String,
    snippet: Option<String>,
    open: Option<String>,
}

impl App {
    /// Opens the box (empty, or with `words`).
    pub(crate) fn openbox_open(&mut self, words: &str) {
        self.find.active = true;
        self.find.query = ChatBox::with_text(Kind::Line, words);
        self.find.sel = 0;
        self.find.hits.clear();
        self.find.hits_for.clear();
        self.find.asked.clear();
        self.find.changed = self.clock;
        self.find.note = None;
    }

    fn find_close(&mut self) {
        self.find.active = false;
        self.find.hits.clear();
    }

    fn find_words(&self) -> String {
        self.find.query.text()
    }

    /// The rows: the held runs that match the words, with the hits of the search merged in, grouped by repository (the newest group first).
    fn find_rows(&self) -> Vec<Row> {
        let words = self.find_words();
        let mut filter = RunsFilter::new();
        filter.set_text(&words);
        let shown: Vec<usize> = filter.visible(&self.runs_rows);
        let mut rows: Vec<Row> = Vec::new();
        let recent = words.trim().is_empty();
        for (n, &i) in shown.iter().enumerate() {
            if recent && n >= RECENT {
                break;
            }
            let r = &self.runs_rows[i];
            rows.push(Row {
                repo: r.project.clone(),
                run_id: r.run_id.clone(),
                title: r.label().unwrap_or_default().to_string(),
                state: r.status.clone(),
                snippet: None,
                open: None,
            });
        }
        if self.find.hits_for == words {
            for h in &self.find.hits {
                let snippet = ham_core::sanitize::display_line(&h.snippet);
                if let Some(row) = rows.iter_mut().find(|r| r.run_id == h.run_id) {
                    row.snippet = Some(snippet);
                    row.open = h.open.clone();
                    continue;
                }
                let held = self.runs_rows.iter().find(|r| r.run_id == h.run_id);
                rows.push(Row {
                    repo: held.map_or_else(|| "other runs".to_string(), |r| r.project.clone()),
                    run_id: h.run_id.clone(),
                    title: held.and_then(|r| r.label()).unwrap_or_default().to_string(),
                    state: held.map(|r| r.status.clone()).unwrap_or_default(),
                    snippet: Some(snippet),
                    open: h.open.clone(),
                });
            }
        }
        // Group by repository in the order each first appears.
        let mut order: Vec<String> = Vec::new();
        for r in &rows {
            if !order.contains(&r.repo) {
                order.push(r.repo.clone());
            }
        }
        let mut grouped = Vec::with_capacity(rows.len());
        for repo in order {
            grouped.extend(rows.iter().filter(|r| r.repo == repo).cloned());
        }
        grouped
    }

    /// A key while the box is open (it takes every key): the words, the mark, Enter, Esc.
    pub(crate) fn openbox_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let rows = self.find_rows();
        let last = rows.len().saturating_sub(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.find_close(),
            KeyCode::Down => self.find.sel = (self.find.sel + 1).min(last),
            KeyCode::Up => self.find.sel = self.find.sel.saturating_sub(1),
            KeyCode::Char('n') if ctrl => self.find.sel = (self.find.sel + 1).min(last),
            KeyCode::Char('p') if ctrl => self.find.sel = self.find.sel.saturating_sub(1),
            KeyCode::Char('u') if ctrl => {
                self.find.query.clear();
                self.find_changed();
            }
            KeyCode::Enter if self.calm => {
                if let Some(row) = rows.get(self.find.sel.min(last)).cloned() {
                    self.find_close();
                    return self.find_open_row(&row);
                }
            }
            KeyCode::Enter => {}
            KeyCode::Char('o') if ctrl => self.find_close(),
            _ => {
                let before = self.find.query.text();
                self.find.query.handle_key(*key);
                if self.find.query.text() != before {
                    self.find_changed();
                }
            }
        }
        vec![]
    }

    fn find_changed(&mut self) {
        self.find.sel = 0;
        self.find.changed = self.clock;
        self.find.note = None;
    }

    /// Opens the run of a row; a chat hit opens the transcript at the match.
    fn find_open_row(&mut self, row: &Row) -> Vec<Effect> {
        self.hist.next_open = row.open.clone();
        if row.open.is_some() && self.focused_run() == Some(row.run_id.as_str()) {
            // The same run again, at another place: the view is built again.
            self.run = None;
        }
        self.open_run(&row.run_id)
    }

    /// The 1 s timer: the chat search, once the words have rested.
    pub(crate) fn openbox_tick(&mut self) -> Vec<Effect> {
        if !self.find.active || self.find.inflight {
            return vec![];
        }
        let words = self.find_words();
        let words = words.trim();
        if words.chars().count() < MIN_QUERY
            || words == self.find.asked
            || self.clock < self.find.changed + REST
        {
            return vec![];
        }
        self.find.inflight = true;
        self.find.asked = words.to_string();
        vec![Effect::Ext(Job::Search {
            query: SearchQuery {
                q: words.to_string(),
                since: None,
                limit: Some(HITS),
                after: None,
            },
        })]
    }

    /// The answer to a search.
    pub(crate) fn openbox_done(
        &mut self,
        query: &str,
        result: Result<ham_core::runsearch::SearchResponse, String>,
    ) {
        self.find.inflight = false;
        if !self.find.active {
            return;
        }
        match result {
            Ok(r) => {
                if query == self.find_words().trim() {
                    self.find.hits = r.hits;
                    self.find.hits_for = self.find_words();
                    self.find.note = r.partial.then(|| {
                        "the search stopped at its time limit: older runs were not searched"
                            .to_string()
                    });
                }
            }
            Err(e) => {
                self.find.note = Some(format!(
                    "the search of conversations failed: {}",
                    sanitize_line(&e)
                ));
            }
        }
    }

    /// Draws the box over the whole screen.
    pub(crate) fn draw_openbox(&self, f: &mut Frame) {
        if !self.find.active {
            return;
        }
        let area: Rect = f.area();
        self.hits.borrow_mut().push(area, Target::Block, "Esc");
        f.render_widget(Clear, area);
        let block = UiBlock::default().borders(Borders::ALL).title(
            " Open anything \u{b7} Enter opens \u{b7} Esc closes \u{b7} \u{2191}\u{2193} move ",
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let rows = self.find_rows();
        let mut lines: Vec<Line> = Vec::new();
        let words = self.find_words();
        lines.push(Line::from(vec![
            Span::styled("\u{25b6} ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(sanitize_line(&words)),
        ]));
        if rows.is_empty() {
            lines.push(Line::from(if words.trim().is_empty() {
                "no runs yet"
            } else {
                "no run matches"
            }));
        }
        let sel = self.find.sel.min(rows.len().saturating_sub(1));
        let mut marked_line = 0usize;
        let mut group: Option<&str> = None;
        for (i, r) in rows.iter().enumerate() {
            if group != Some(r.repo.as_str()) {
                group = Some(r.repo.as_str());
                lines.push(Line::from(Span::styled(
                    sanitize_line(&r.repo),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
            }
            let mark = if i == sel { "> " } else { "  " };
            let title = if r.title.is_empty() {
                String::new()
            } else {
                format!("  {}", sanitize_line(&r.title))
            };
            let state = if r.state.is_empty() {
                String::new()
            } else {
                format!("  [{}]", sanitize_line(&r.state))
            };
            if i == sel {
                marked_line = lines.len();
            }
            lines.push(Line::from(format!(
                "{mark}{}{title}{state}",
                sanitize_line(&r.run_id)
            )));
            if let Some(s) = &r.snippet {
                lines.push(Line::from(Span::styled(
                    format!("    {}", sanitize_line(s)),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
        if let Some(n) = &self.find.note {
            lines.push(Line::from(Span::styled(
                sanitize_line(n),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        // Scroll so the marked row is on screen.
        let room = usize::from(inner.height);
        let skip = (marked_line + 1).saturating_sub(room);
        let shown: Vec<Line> = lines.into_iter().skip(skip).take(room).collect();
        f.render_widget(Paragraph::new(shown), inner);
        if skip == 0 {
            let typed = u16::try_from(words.chars().count()).unwrap_or(u16::MAX);
            f.set_cursor_position((
                inner
                    .x
                    .saturating_add(2)
                    .saturating_add(typed)
                    .min(inner.right().saturating_sub(1)),
                inner.y,
            ));
        }
    }

    /// An answer that is the box's.
    pub(crate) fn openbox_message(&mut self, done: Done) {
        if let Done::Searched { query, result } = done {
            self.openbox_done(&query, result);
        }
    }
}

impl App {
    /// The keys of the chat-client features that come before the screen's own: the open-anything box takes every key while it is open and
    /// opens on Ctrl-O; the run screen's previews (`/now`, a held message, the follow-up) and the keys `w` and `x`.
    pub(crate) fn ext_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        // The tag explorer takes every key while it is open, the open-anything box included.
        if self.tags.open && self.screen == Screen::Runs {
            return Some(self.tags_key(key));
        }
        if self.tags.open {
            // The Runs list was left under the overlay (a run was opened some other way): it is closed, not left over another screen.
            self.tags_close();
        }
        if self.find.active {
            return Some(self.openbox_key(key));
        }
        if keys::opens_anything(key) && matches!(self.screen, Screen::Runs | Screen::Dashboard) {
            self.openbox_open("");
            return Some(vec![]);
        }
        if self.screen != Screen::Dashboard {
            return None;
        }
        // A `/tag` line waits for its second Enter.
        if let Some(effects) = self.tags_confirm_key(key) {
            return Some(effects);
        }
        match self.mode {
            Mode::Typing => self.send_typing_key(key),
            Mode::Browse => self.send_browse_key(key),
        }
    }

    /// An answer of a chat-client request.
    pub(crate) fn ext_done(&mut self, done: Done) -> Vec<Effect> {
        match done {
            Done::Transcript {
                run_id,
                read,
                result,
            } => self.hist_done(&run_id, read, result),
            d @ Done::Searched { .. } => {
                self.openbox_message(d);
                vec![]
            }
            Done::FullPrompt { run_id, result } => self.earlier_full_done(&run_id, result),
            d => self.send_done(d),
        }
    }
}
