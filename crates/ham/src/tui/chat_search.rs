//! Search inside a chat, joined to the transcript. [`ChatSearchUi`] holds a `ham_core::search::ChatSearch` and carries out
//! what it asks of a [`TranscriptState`]:
//!
//! * a match in a folded block, head or body, opens the block (and closes the one it opened before, so stepping with `n` leaves no trail);
//!   cancelling with Esc while typing closes it again and puts the scroll position back; accepting with Enter hands the open block to the
//!   user, who keeps it open;
//! * the hit is brought to the middle of the view ([`TranscriptState::reveal_text`]) and every hit is painted ([`TranscriptState::set_marks`]),
//!   the current one in another colour;
//! * [`SearchBar`] draws the one-line bar (`/query  3/17`).
//!
//! The module reads no events and holds no `App`: the TUI-core owner maps keys to [`ChatSearchUi::key`] while
//! [`ChatSearchUi::takes_text`] (every character, an `n` too, is the query's), `n` and `N` to [`ChatSearchUi::next`] and
//! [`ChatSearchUi::prev`] only while [`ChatSearchUi::walks_matches`] (after Enter), gives Esc to the search first while
//! [`ChatSearchUi::is_active`], calls [`ChatSearchUi::model_changed`] after it hands the chat view a new model, and
//! [`ChatSearchUi::close`] when it leaves the chat.

use super::transcript::{Mark, TranscriptState};
use ham_core::chat::ChatModel;
use ham_core::sanitize::display_line;
use ham_core::search::{ChatSearch, Effect, Hit, Key, Phase, ScrollMark};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use std::collections::HashMap;

/// Where the view is now, for a search that opens: the scroll position, follow mode, the selected block and the block on the first row
/// of `area` (the area the transcript was last drawn into).
pub fn mark_of(state: &TranscriptState, area: Rect) -> ScrollMark {
    ScrollMark {
        top_line: state.top_line(),
        following: state.is_following(),
        selected: state.selected_id().map(str::to_string),
        // The first row that belongs to a block: the progress-feed label above the blocks is not one.
        anchor: (0..area.height).find_map(|dy| state.hit(area.x, area.y + dy).map(|h| h.index)),
    }
}

/// A chat search and the folded block it holds open.
#[derive(Debug, Default)]
pub struct ChatSearchUi {
    search: ChatSearch,
    /// The block this search opened (it was folded), to close it when the search moves on or is cancelled.
    opened: Option<String>,
}

impl ChatSearchUi {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn search(&self) -> &ChatSearch {
        &self.search
    }

    /// The bar is open or matches are marked: Esc is the search's before it is anything else's. It is true while typing too; use
    /// [`walks_matches`](Self::walks_matches) for `n` and `N` and [`takes_text`](Self::takes_text) for characters.
    pub fn is_active(&self) -> bool {
        self.search.is_active()
    }

    /// The bar is open and every character is the query's.
    pub fn takes_text(&self) -> bool {
        self.search.takes_text()
    }

    /// The search was accepted: `n` and `N` walk the matches.
    pub fn walks_matches(&self) -> bool {
        self.search.walks_matches()
    }

    /// Opens the bar over the view that `state` shows in `area`. A bar that is already open is cancelled first: the block it opened
    /// is closed again.
    pub fn open(&mut self, state: &mut TranscriptState, model: &ChatModel, area: Rect) {
        self.close_ours(state, model);
        state.cancel_reveal_text();
        self.search.open(mark_of(state, area));
    }

    /// Leaves the chat: the search, its marks and a jump not yet drawn are dropped; the blocks stay as the user last saw them.
    pub fn close(&mut self, state: &mut TranscriptState) {
        self.search = ChatSearch::new();
        self.opened = None;
        state.clear_marks();
        state.cancel_reveal_text();
    }

    /// A key of the bar (typing), or Esc on an accepted search.
    pub fn key(&mut self, key: Key, state: &mut TranscriptState, model: &ChatModel) {
        let was_typing = self.search.takes_text();
        let effects = self.search.key(key, model);
        if was_typing && self.search.walks_matches() {
            // Accepted: the block the search opened is the user's now.
            self.opened = None;
        }
        self.apply(effects, state, model);
    }

    /// `n`.
    pub fn next(&mut self, state: &mut TranscriptState, model: &ChatModel) {
        let effects = self.search.next(model);
        self.apply(effects, state, model);
    }

    /// `N`.
    pub fn prev(&mut self, state: &mut TranscriptState, model: &ChatModel) {
        let effects = self.search.prev(model);
        self.apply(effects, state, model);
    }

    /// The chat changed: the matches are searched again and painted again; the view does not move.
    pub fn model_changed(&mut self, state: &mut TranscriptState, model: &ChatModel) {
        self.search.model_changed(model);
        self.apply(Vec::new(), state, model);
    }

    fn apply(&mut self, effects: Vec<Effect>, state: &mut TranscriptState, model: &ChatModel) {
        for e in effects {
            match e {
                Effect::Show(hit) => self.show(&hit, state, model),
                Effect::Restore(mark) => self.restore(&mark, state, model),
            }
        }
        if !self.search.is_active() {
            // A cleared search lets go of its block: what the user read there stays open.
            self.opened = None;
        }
        self.paint(state);
    }

    fn close_ours(&mut self, state: &mut TranscriptState, model: &ChatModel) {
        if let Some(id) = self.opened.take() {
            state.close_fold(model, &id);
        }
    }

    fn show(&mut self, hit: &Hit, state: &mut TranscriptState, model: &ChatModel) {
        // A folded block shows its head cut to one row and no body: a match in either is found open.
        if self.opened.as_deref() != Some(hit.id.as_str()) {
            self.close_ours(state, model);
        }
        if self.opened.is_none() && state.open_fold(model, &hit.id) {
            self.opened = Some(hit.id.clone());
        }
        state.reveal_text(&hit.id, hit.nb_start);
    }

    fn restore(&mut self, mark: &ScrollMark, state: &mut TranscriptState, model: &ChatModel) {
        self.close_ours(state, model);
        state.cancel_reveal_text();
        if mark.following {
            state.to_bottom();
        } else {
            state.scroll_to_line(mark.top_line);
        }
    }

    /// Hands the transcript the marks of the hits (none when the search is closed).
    fn paint(&self, state: &mut TranscriptState) {
        if self.search.phase() == Phase::Idle {
            state.clear_marks();
            return;
        }
        let current = self.search.current();
        let mut marks: HashMap<String, Vec<Mark>> = HashMap::new();
        for (i, h) in self.search.hits().iter().enumerate() {
            if h.nb_len == 0 {
                continue;
            }
            marks.entry(h.id.clone()).or_default().push(Mark {
                nb_start: h.nb_start,
                nb_len: h.nb_len,
                current: Some(i) == current,
            });
        }
        state.set_marks(marks);
    }
}

/// The one-line search bar: `/query  3/17`, the end of the query kept when the row is short. It draws nothing while no search is open.
#[derive(Debug)]
pub struct SearchBar<'a> {
    search: &'a ChatSearch,
}

impl<'a> SearchBar<'a> {
    pub fn new(search: &'a ChatSearch) -> Self {
        Self { search }
    }
}

impl Widget for SearchBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let text = display_line(&self.search.bar_text(usize::from(area.width)));
        let style = if self.search.phase() == Phase::Editing {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        buf.set_stringn(area.x, area.y, &text, usize::from(area.width), style);
    }
}
