//! The run chat view: the screen of one run, drawn into a `Rect`.
//!
//! From the top: a [`Header`] line (run, model, state, elapsed time, tokens, cost, age of the last read), the run's name on a row of its own,
//! an optional banner, the transcript,
//! the rows of messages still on their way ([`PendingRow`]), the pinned input box, a notice row and one footer line.
//!
//! The view holds the model and the transcript's scroll state and draws; it reads no events and holds no `App`. The TUI-core owner wires
//! keys to [`ChatView::scroll`], [`ChatView::select`] and [`ChatView::toggle_selected_fold`], mouse presses to [`ChatView::click`], and
//! builds the [`ChatFacts`] and [`Card`]s from the strings the app already has (the delivery card with its one-line link fitted to
//! [`ChatView::card_width`], the question card, the blocked panel). The input box is whatever implements [`InputPane`]: the frozen
//! `ChatBox` signatures `rows_at` and `render`.
//!
//! Every string handed to the view is cleaned again here (controls and escape sequences go, bidi marks are shown), and nothing is
//! written outside the area given to [`ChatView::draw`].

use super::transcript::{CONTENT_X, Transcript, TranscriptState};
use ham_core::chat::{Block, ChatModel};
use ham_core::notice::NoticeKind;
use ham_core::redact::redact_tokens;
use ham_core::sanitize::display_line;
use ham_core::tables::compact_tokens;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, Borders, Paragraph};
use std::cell::{Cell, RefCell};

/// The rows the conversation keeps when the screen has them.
const MIN_TRANSCRIPT: u16 = 3;
/// The most pending-message rows shown above the box.
const MAX_PENDING: u16 = 3;
/// The gutter of a pending row: the same nine columns as the transcript's.
const PENDING_GUTTER: usize = CONTENT_X as usize;
/// The narrowest screen that has a name row.
const NAME_MIN_WIDTH: usize = 20;

fn safe(text: &str) -> String {
    redact_tokens(&display_line(text)).trim().to_string()
}

/// `text` cut to `max` characters with an ellipsis in place of the rest.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    if max > 0 {
        out.push('\u{2026}');
    }
    out
}

/// `text` in at most `width` terminal columns: whole when it fits, else its start and an ellipsis as the last column (a wide character is
/// never split, so a name of Chinese or Japanese characters is cut by the columns it takes, not by its characters).
fn cut_cols(text: &str, width: usize) -> String {
    let cols = |c: char| ratatui::text::Span::raw(c.to_string()).width();
    if text.chars().map(cols).sum::<usize>() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = cols(c);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// The tags that follow the name on a name row `width` columns wide, as one text: the largest number of whole tags that fits after the
/// name and the two spaces that separate them, then ` +N` for the tags left out (`+N` alone when no whole tag fits). Empty when the name
/// takes the row, when there is no room for even `+N`, or when there is no tag. Each tag is cleaned like every string the view is given.
fn name_row_tags(name: &str, tags: &[String], width: usize) -> String {
    let tags: Vec<String> = tags
        .iter()
        .map(|t| safe(t))
        .filter(|t| !t.is_empty())
        .collect();
    let used = ratatui::text::Span::raw(name.to_string()).width() + 2;
    let Some(room) = width.checked_sub(used).filter(|r| *r > 0) else {
        return String::new();
    };
    let cols = |t: &str| ratatui::text::Span::raw(t.to_string()).width();
    let more = |left: usize| format!("+{left}");
    // `fits[k]`: the first k tags and the `+N` for the rest take at most `room` columns.
    let mut best = None;
    let mut taken = 0usize;
    for k in 0..=tags.len() {
        if k > 0 {
            taken += cols(&tags[k - 1]) + usize::from(k > 1);
        }
        let rest = tags.len() - k;
        let tail = if rest == 0 {
            0
        } else {
            cols(&more(rest)) + usize::from(k > 0)
        };
        if taken + tail <= room {
            best = Some(k);
        }
    }
    let Some(k) = best else {
        return String::new();
    };
    let mut out = tags[..k].join(" ");
    if k < tags.len() {
        if k > 0 {
            out.push(' ');
        }
        out.push_str(&more(tags.len() - k));
    }
    out
}

// ---- header ------------------------------------------------------------------------------------------------------------------------

/// What the header line says about the run. The app fills it from the run view and the run detail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    pub run_id: String,
    /// The model's display name.
    pub model: Option<String>,
    /// `active`, `needs you`, `complete`, `blocked`, ... as the app words it.
    pub state: String,
    pub elapsed_s: Option<u64>,
    pub tokens: Option<u64>,
    /// The cost as the card writes it (`$0.14`, `USD unknown`).
    pub cost: Option<String>,
    /// The turns that have ended: the tokens and the cost are the totals as of the last of them (Cline sends no usage while a turn runs).
    pub as_of_turn: Option<usize>,
    /// Seconds since the last read of the run's events.
    pub updated_ago_s: Option<u64>,
}

/// The longest run id the header writes whole; a ULID has 26 characters.
const MAX_ID: usize = 40;

/// The run id as the header writes it: the whole id, because the live drivers and the people who copy it read it from the title. Only
/// an id too long to be one is cut.
fn short_id(id: &str) -> String {
    cut(&safe(id), MAX_ID)
}

fn elapsed_text(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn age_text(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

impl Header {
    /// The header as one line of at most `width` characters. Too long, it gives up its tail first (the age, the cost, the tokens, the
    /// elapsed time, the model) and only then cuts the line itself.
    pub fn line(&self, width: usize) -> String {
        let id = format!("ham \u{b7} Run {}", short_id(&self.run_id));
        let model = self.model.as_deref().map(safe).filter(|m| !m.is_empty());
        let state = Some(safe(&self.state)).filter(|s| !s.is_empty());
        let elapsed = self.elapsed_s.map(elapsed_text);
        let tokens = self.tokens.map(|t| {
            format!(
                "{} tok",
                compact_tokens(u32::try_from(t).unwrap_or(u32::MAX))
            )
        });
        let cost = self.cost.as_deref().map(safe).filter(|c| !c.is_empty());
        let as_of = self
            .as_of_turn
            .filter(|_| tokens.is_some() || cost.is_some())
            .map(|k| format!("as of turn {k}"));
        let updated = self
            .updated_ago_s
            .map(|s| format!("updated {} ago", age_text(s)));
        // Slots in line order; the tail is given up from the end, the model last.
        let mut slots: [Option<String>; 8] = [
            Some(id),
            model,
            state,
            elapsed,
            tokens,
            cost,
            as_of,
            updated,
        ];
        let join = |slots: &[Option<String>; 8]| -> String {
            slots
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(" \u{b7} ")
        };
        for drop in [7usize, 6, 5, 4, 3, 1] {
            let line = join(&slots);
            if line.chars().count() <= width {
                return line;
            }
            slots[drop] = None;
        }
        cut(&join(&slots), width)
    }
}

// ---- layout ------------------------------------------------------------------------------------------------------------------------

/// The rows of the screen, top to bottom: header, name, banner, transcript, pending rows, input box, notice, footer. A part that has no room
/// has height 0; the parts tile the area exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Areas {
    pub header: Rect,
    /// The run's name: one row directly under the header, or none.
    pub name: Rect,
    pub banner: Rect,
    pub transcript: Rect,
    pub pending: Rect,
    /// Rows between the pending rows and the box that the app draws itself (a command's result panel and its preview); empty unless
    /// [`ChatView::draw_with`] asked for some.
    pub reserved: Rect,
    pub input: Rect,
    pub notice: Rect,
    pub footer: Rect,
}

/// The run screen in the title band (`head.rs`): where the header is drawn and the rows the box keeps a third of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    /// The band's title rectangle (row 0, from the head's prefix to the right edge).
    pub title: Rect,
    /// The rows below the band: the box keeps at most a third of them.
    pub cap_base: u16,
}

/// Splits `area`. `input_rows` is the number of text rows the box wants (its borders come on top); the box keeps at most a third of
/// the screen. On a short screen the conversation keeps [`MIN_TRANSCRIPT`] rows first, then the box, the notice, the banner and the
/// pending rows take what is left in that order.
pub fn areas(area: Rect, banner: bool, pending_rows: u16, input_rows: u16, notice: bool) -> Areas {
    areas_with(area, banner, pending_rows, input_rows, notice, 0)
}

/// [`areas`] with `reserved_rows` rows between the pending rows and the box for the app's own use. They are taken right after the
/// box, before the notice, the banner and the pending rows: what asks the user for a key must stay on a short screen.
pub fn areas_with(
    area: Rect,
    banner: bool,
    pending_rows: u16,
    input_rows: u16,
    notice: bool,
    reserved_rows: u16,
) -> Areas {
    areas_named(
        area,
        banner,
        pending_rows,
        input_rows,
        notice,
        reserved_rows,
        false,
    )
}

/// [`areas_with`] with a row for the run's name directly under the header. It is taken last, after the pending rows, so a short
/// screen drops it first and the conversation keeps its [`MIN_TRANSCRIPT`] rows; every other part gets what it gets without the row.
pub fn areas_named(
    area: Rect,
    banner: bool,
    pending_rows: u16,
    input_rows: u16,
    notice: bool,
    reserved_rows: u16,
    name: bool,
) -> Areas {
    areas_capped(
        area,
        area.height,
        banner,
        pending_rows,
        input_rows,
        notice,
        reserved_rows,
        name,
    )
}

/// [`areas_named`] with the box capped at a third of `cap_base` rows instead of a third of the area's own height. The run screen in the title
/// band lays itself out in an area that starts two rows into the band (its header row is the band's last row, `head.rs`), and the box keeps
/// a third of the rows below the band, so the cap is told apart from the area. With `cap_base` equal to the area's height this is
/// [`areas_named`], for every area and every combination (`tests/tui_head.rs` compares it with the layout as it was).
#[allow(clippy::too_many_arguments)]
pub fn areas_capped(
    area: Rect,
    cap_base: u16,
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
    let reserve = take(&mut rem, MIN_TRANSCRIPT);
    let box_rows = input_rows.clamp(1, (cap_base / 3).max(1));
    let input_h = take(&mut rem, box_rows + 2);
    let reserved_h = take(&mut rem, reserved_rows);
    let notice_h = take(&mut rem, u16::from(notice));
    let banner_h = take(&mut rem, u16::from(banner));
    let pending_h = take(&mut rem, pending_rows.min(MAX_PENDING));
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

// ---- cards and pending rows ------------------------------------------------------------------------------------------------------

/// One card of the run screen (status and agents, the question, the blocked panel, the delivery card) as a block at the end of the
/// transcript: `title` is its head line, `lines` its body. An `alert` card is the question or the blocked reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
    /// Starts folded (the status and agent cards); the delivery card and the question start open.
    pub folded: bool,
    pub alert: bool,
}

impl Card {
    fn block(&self) -> Block {
        let kind = if self.alert {
            NoticeKind::NeedsInput
        } else {
            NoticeKind::RunStatus
        };
        Block::card(
            &format!("card:{}", self.id),
            &self.title,
            &self.lines.join("\n"),
            kind,
            self.folded,
        )
    }
}

/// Where a message the user sent stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingStatus {
    Sending,
    WaitingForTurn,
    Delivered {
        turn: u32,
    },
    Failed(String),
    /// The words of any other state (held, withdrawn, rejected, superseded, may have reached the engine ...), written by `ham_core::messages`.
    Words(String),
}

/// A message on its way to Cline, drawn above the box until it is in the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRow {
    pub text: String,
    pub status: PendingStatus,
}

impl PendingRow {
    fn status_words(&self) -> String {
        match &self.status {
            PendingStatus::Sending => "sending".to_string(),
            PendingStatus::WaitingForTurn => "waits for this turn to end".to_string(),
            PendingStatus::Delivered { turn } => format!("delivered in turn {turn}"),
            PendingStatus::Failed(why) => format!("not delivered: {}", safe(why)),
            PendingStatus::Words(words) => safe(words),
        }
    }

    /// `you      <text> — <status>` in `width` columns; the text gives way to the status.
    fn line(&self, width: usize) -> String {
        let status = format!(" \u{2014} {}", self.status_words());
        let room = width
            .saturating_sub(PENDING_GUTTER)
            .saturating_sub(status.chars().count());
        let text = cut(&safe(&self.text), room);
        cut(
            &format!("{:<w$}{text}{status}", "you", w = PENDING_GUTTER),
            width,
        )
    }
}

// ---- the view ------------------------------------------------------------------------------------------------------------------------

/// What the app says about the screen besides the conversation.
#[derive(Clone, Copy, Debug)]
pub struct ChatFacts<'a> {
    pub header: &'a Header,
    /// A line that reverses the row under the header (`reconnecting`).
    pub banner: Option<&'a str>,
    /// The notice row between the box and the footer (an outcome, a confirm line).
    pub notice: Option<&'a str>,
    pub pending: &'a [PendingRow],
    /// The footer line: the keys that work now.
    pub footer: &'a str,
    /// The title of the input box.
    pub box_title: &'a str,
    /// The box has the keyboard.
    pub focused: bool,
    /// Columns at the right end of the header row that the app draws over (the version tag and its gap): the header fits itself to the rest.
    pub header_reserve: usize,
    /// The run's name and whether a model wrote it: drawn on the row under the header, plain for a model's name and dim for the seed
    /// built from the prompt. `None`, or a screen under 20 columns, has no name row.
    pub name: Option<(&'a str, bool)>,
    /// The run's effective tags, drawn dim after the name on the name row: whole tags first, then `+N` for the rest. The name has
    /// priority, so a name that fills the row leaves no tag; a screen with no name row, or a run with no name, draws none.
    pub tags: &'a [String],
}

impl<'a> ChatFacts<'a> {
    pub fn new(header: &'a Header, footer: &'a str) -> Self {
        Self {
            header,
            banner: None,
            notice: None,
            pending: &[],
            footer,
            box_title: "Message",
            focused: false,
            header_reserve: 0,
            name: None,
            tags: &[],
        }
    }
}

/// The input box, as the chat view needs it. `ChatBox` implements it with its `rows_at` and `render`.
pub trait InputPane {
    /// Display rows the text needs at `inner_width`, at least 1.
    fn rows_at(&self, inner_width: usize) -> usize;
    /// Draws the box into `area` inside `block` (the border and title the view chose); when `focused` it puts the cursor at the caret.
    fn render(&self, f: &mut Frame, area: Rect, block: UiBlock<'_>, focused: bool);
}

/// How the browse keys move the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAction {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    /// The end, and follow mode again.
    Bottom,
}

/// Which block the selection moves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Next,
    Prev,
}

/// The conversation and its scroll state. `draw` takes `&self` (the app draws from `&App`); the scroll state sits in a `RefCell`.
#[derive(Debug)]
pub struct ChatView {
    /// The model's blocks followed by the cards.
    model: ChatModel,
    /// How many of `model.blocks` are the model's own; the rest are cards.
    n_model: usize,
    state: RefCell<TranscriptState>,
    /// The transcript's area at the last draw.
    area: Cell<Rect>,
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            model: ChatModel::from_blocks(ham_core::chat::Source::FullHistory, "", Vec::new()),
            n_model: 0,
            state: RefCell::new(TranscriptState::new()),
            area: Cell::new(Rect::default()),
        }
    }

    /// The columns of a card line that never wraps in an area `area_width` columns wide: the delivery card's one-line link is fitted to it.
    pub fn card_width(area_width: u16) -> usize {
        usize::from(area_width.saturating_sub(CONTENT_X + 1))
    }

    /// A new model (the events read so far); the cards stay at its end.
    pub fn set_model(&mut self, model: ChatModel) {
        let cards = self.model.blocks.split_off(self.n_model);
        self.n_model = model.blocks.len();
        self.model = model;
        self.model.blocks.extend(cards);
    }

    /// The cards at the end of the conversation, in order. The same cards again change nothing.
    pub fn set_cards(&mut self, cards: &[Card]) {
        let blocks: Vec<Block> = cards.iter().map(Card::block).collect();
        if self.model.blocks[self.n_model..] == blocks[..] {
            return;
        }
        self.model.blocks.truncate(self.n_model);
        self.model.blocks.extend(blocks);
    }

    /// The model with the cards.
    pub fn model(&self) -> &ChatModel {
        &self.model
    }

    /// A selection begins at the cell `(x, y)` of the last draw.
    pub fn select_start(&self, x: u16, y: u16) {
        self.state.borrow_mut().select_start(x, y);
    }

    /// The end of the selection the pointer holds moves to the cell `(x, y)`.
    pub fn select_extend(&self, x: u16, y: u16) {
        self.state.borrow_mut().select_extend(x, y);
    }

    /// The selected text as it was written, if there is a selection whose rows are still held.
    pub fn selection_text(&self) -> Option<String> {
        self.state.borrow().selection_text()
    }

    pub fn clear_selection(&self) {
        self.state.borrow_mut().clear_selection();
    }

    pub fn has_selection(&self) -> bool {
        self.state.borrow().has_selection()
    }

    /// The conversation alone: the model's own blocks, without the cards of the run screen.
    pub fn conversation(&self) -> ChatModel {
        let mut model = self.model.clone();
        model.blocks.truncate(self.n_model);
        model
    }

    pub fn scroll(&mut self, action: ScrollAction) {
        let mut s = self.state.borrow_mut();
        match action {
            ScrollAction::LineUp => s.scroll_up(1),
            ScrollAction::LineDown => s.scroll_down(1),
            ScrollAction::PageUp => s.page_up(),
            ScrollAction::PageDown => s.page_down(),
            ScrollAction::Top => s.to_top(),
            ScrollAction::Bottom => s.to_bottom(),
        }
    }

    pub fn select(&mut self, step: Step) {
        let mut s = self.state.borrow_mut();
        match step {
            Step::Next => s.select_next(&self.model),
            Step::Prev => s.select_prev(&self.model),
        }
    }

    pub fn selected_id(&self) -> Option<String> {
        self.state.borrow().selected_id().map(str::to_string)
    }

    pub fn toggle_selected_fold(&mut self) {
        self.state.borrow_mut().toggle_selected_fold(&self.model);
    }

    /// The head rows of the foldable blocks that are on screen at the last draw (rectangle and block id): the regions a press toggles.
    pub fn fold_heads(&self) -> Vec<(Rect, String)> {
        self.state.borrow().fold_heads()
    }

    /// Toggles the fold of the block `id` (a press on its head).
    pub fn toggle_fold(&mut self, id: &str) {
        self.state.borrow_mut().toggle_fold(&self.model, id);
    }

    /// Scrolls the conversation by `lines` (positive: toward newer text), as the wheel does.
    pub fn scroll_lines(&mut self, lines: i32) {
        let n = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        let mut state = self.state.borrow_mut();
        if lines >= 0 {
            state.scroll_down(n);
        } else {
            state.scroll_up(n);
        }
    }

    /// A press at the screen position `(x, y)` of the last draw: it folds or opens a foldable block of the conversation.
    pub fn click(&mut self, x: u16, y: u16) -> bool {
        self.state.borrow_mut().click(&self.model, x, y)
    }

    /// The transcript's area at the last draw: a search needs it to tell which block is on the first row.
    pub fn transcript_area(&self) -> Rect {
        self.area.get()
    }

    /// Runs `f` on the scroll state and the model. Search inside the chat changes folds, marks and the scroll position through
    /// this; the rest of the app uses the methods above.
    pub fn with_state<R>(&mut self, f: impl FnOnce(&mut TranscriptState, &ChatModel) -> R) -> R {
        f(self.state.get_mut(), &self.model)
    }

    pub fn is_following(&self) -> bool {
        self.state.borrow().is_following()
    }

    /// The whole conversation fits the screen at the last draw.
    pub fn fits(&self) -> bool {
        self.state.borrow().fits()
    }

    /// The view is scrolled to the very top (not following the end).
    pub fn at_top(&self) -> bool {
        let s = self.state.borrow();
        !s.is_following() && s.top_line() == 0
    }

    /// Blocks that arrived while follow mode was paused.
    pub fn new_blocks(&self) -> usize {
        self.state.borrow().new_blocks()
    }

    pub fn range_text(&self) -> String {
        self.state.borrow().range_text()
    }

    /// Blocks laid out so far (see [`TranscriptState::wrap_computations`]).
    pub fn wrap_computations(&self) -> u64 {
        self.state.borrow().wrap_computations()
    }

    /// Draws the screen of the run into `area`.
    pub fn draw(&self, f: &mut Frame, area: Rect, facts: &ChatFacts<'_>, input: &dyn InputPane) {
        self.draw_with(f, area, facts, input, 0);
    }

    /// [`ChatView::draw`] with `reserved_rows` rows kept free between the pending rows and the box. The returned [`Areas`] names them
    /// (`reserved`): the app draws there after this call. Nothing is drawn in them here.
    pub fn draw_with(
        &self,
        f: &mut Frame,
        area: Rect,
        facts: &ChatFacts<'_>,
        input: &dyn InputPane,
        reserved_rows: u16,
    ) -> Areas {
        self.draw_in(f, area, None, facts, input, reserved_rows)
    }

    /// [`ChatView::draw_with`] in the title band (`head.rs`): `band` says where the header goes (the band's title rectangle, whose width the
    /// header fits itself to, less `facts.header_reserve`) and what the box keeps a third of. `area` is the one `Split::area` gives, whose
    /// first row is the band's last row: the header's own row there is not drawn, the name row is the first row below the band and the
    /// returned [`Areas`] names the band's title as the header.
    pub fn draw_in(
        &self,
        f: &mut Frame,
        area: Rect,
        band: Option<Band>,
        facts: &ChatFacts<'_>,
        input: &dyn InputPane,
        reserved_rows: u16,
    ) -> Areas {
        let width = usize::from(area.width);
        let rows = u16::try_from(input.rows_at(width.saturating_sub(2))).unwrap_or(u16::MAX);
        let banner = facts.banner.map(safe).filter(|b| !b.is_empty());
        let notice = facts.notice.map(safe).filter(|n| !n.is_empty());
        let pending: &[PendingRow] = {
            let n = facts.pending.len().saturating_sub(usize::from(MAX_PENDING));
            &facts.pending[n..]
        };
        let name = facts
            .name
            .map(|(n, ai)| (safe(n), ai))
            .filter(|(n, _)| !n.is_empty() && width >= NAME_MIN_WIDTH);
        let mut a = areas_capped(
            area,
            band.map_or(area.height, |b| b.cap_base),
            banner.is_some(),
            u16::try_from(pending.len()).unwrap_or(u16::MAX),
            rows,
            notice.is_some(),
            reserved_rows,
            name.is_some(),
        );
        self.area.set(a.transcript);
        // The header's own row, or in the band the band's title rectangle; either way the header fits the width of the row it is drawn in.
        let header_at = band.map_or(a.header, |b| b.title);
        if a.header.height > 0 && header_at.width > 0 && header_at.height > 0 {
            f.render_widget(
                Paragraph::new(
                    Line::from(
                        facts.header.line(
                            usize::from(header_at.width).saturating_sub(facts.header_reserve),
                        ),
                    )
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                ),
                header_at,
            );
        }
        a.header = header_at;
        if let (Some((n, ai)), true) = (&name, a.name.height > 0) {
            let style = if *ai {
                Style::default()
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            let tags = name_row_tags(n, facts.tags, width);
            let line = if tags.is_empty() {
                Line::from(cut_cols(n, width))
            } else {
                Line::from(vec![
                    Span::raw(n.clone()),
                    Span::styled(
                        format!("  {tags}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ])
            };
            f.render_widget(Paragraph::new(line).style(style), a.name);
        }
        if let (Some(b), true) = (&banner, a.banner.height > 0) {
            f.render_widget(
                Paragraph::new(cut(b, width))
                    .style(Style::default().add_modifier(Modifier::REVERSED)),
                a.banner,
            );
        }
        if a.transcript.height > 0 {
            f.render_stateful_widget(
                Transcript::new(&self.model),
                a.transcript,
                &mut *self.state.borrow_mut(),
            );
        }
        let dim = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
        for (i, row) in pending
            .iter()
            .enumerate()
            .take(usize::from(a.pending.height))
        {
            let r = Rect::new(
                a.pending.x,
                a.pending.y + u16::try_from(i).unwrap_or(u16::MAX),
                a.pending.width,
                1,
            );
            f.render_widget(Paragraph::new(row.line(width)).style(dim), r);
        }
        if a.input.height > 0 {
            let title = safe(facts.box_title);
            let emphasis = if facts.focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let block = UiBlock::default()
                .borders(Borders::ALL)
                .title(Line::from(format!(" {title} ")).style(emphasis))
                .border_style(emphasis);
            input.render(f, a.input, block, facts.focused);
        }
        if let (Some(n), true) = (&notice, a.notice.height > 0) {
            f.render_widget(Paragraph::new(cut(n, width)), a.notice);
        }
        if a.footer.height > 0 {
            f.render_widget(Paragraph::new(cut(&safe(facts.footer), width)), a.footer);
        }
        a
    }
}
