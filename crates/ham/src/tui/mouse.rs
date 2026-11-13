//! The pure part of the mouse: a hit map that every draw fills, and a pointer state machine that turns crossterm
//! mouse events into actions. The module reads no terminal, draws nothing and holds no `App`; the clock is passed in, so tests step it
//! instead of sleeping. Whether the terminal reports the mouse at all is the session guard's policy (`session::wants_mouse`: on by
//! default, `mouse = false` and `HAMH_NO_MOUSE=1` switch it off, `auto` is a third value that is not the default); with no reports there
//! are no events and this module is idle.
//!
//! * **Hit map.** [`HitMap::begin`] at the start of a draw, then [`HitMap::push`] a rectangle, a [`Target`] and the name of the key that
//!   does the same, for every box, run row, fold, key chip, link and text pane drawn. A region drawn later wins where two overlap, so a
//!   popup drawn over a list takes the click ([`Target::Block`] shields what lies under it). Regions are clipped to the frame.
//!   [`HitMap::resized`] makes the map stale until the next `begin`: a click between a resize and the next draw hits nothing.
//!   Every click has a key: `push` takes the key's name and [`HitMap::unkeyed`] lists the regions that name none, for a test.
//! * **Pointer.** A left press on a box focuses it, on a run row selects it (a second click on the same run within [`DOUBLE_CLICK`]
//!   opens it), on a key chip presses that key, on a link opens it. A fold sits on top of the selectable text, so a press there waits
//!   for the button to come up: on the same cell it toggles the fold (if that fold is still under the pointer), after a move it is a
//!   text selection. While text is selected the next press only clears the selection, except that a press on selectable text clears
//!   it and may start a new one (dragging is not a click). The wheel scrolls the pane under the pointer by a step, faster for a fast
//!   run of notches. Other buttons, motion, sideways wheels and modifiers change nothing.
//! * **What cancels.** A key press ([`Pointer::note_key`]), another target, a wheel and a resize break a double click; a resize or a map
//!   that changes epoch drops a drag and a selection. A focus-lost event is the screen's to turn into [`Pointer::reset`].

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};

/// A second click on the same run inside this window opens it.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// Lines a wheel notch scrolls unless the user chose another step.
pub const DEFAULT_WHEEL_STEP: u16 = 3;
/// The largest step a user may choose.
pub const MAX_WHEEL_STEP: u16 = 30;
/// Notches closer together than this, in one direction, count as a fast run.
pub const ACCEL_GAP: Duration = Duration::from_millis(60);
/// A fast run adds one step to every notch after this many notches, up to [`ACCEL_MAX`] times the step.
pub const ACCEL_EVERY: u32 = 4;
/// The most a fast run multiplies the step by.
pub const ACCEL_MAX: u32 = 4;
/// While the button is held outside a pane, [`Pointer::tick`] scrolls it this often.
pub const AUTOSCROLL_EVERY: Duration = Duration::from_millis(60);

/// A text box of the screen (the screen numbers its boxes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BoxId(pub u16);

/// A list of the screen (the Runs list, the Artifacts list, a popup).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ListId(pub u16);

/// A pane that scrolls (the transcript, a list).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u16);

/// What a region of the screen is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A text box: a press focuses it.
    Box(BoxId),
    /// A row of a list. `id` is a stable identity of what the row shows (see [`stable_id`]), so a second click counts only on the same
    /// run even if the list was sorted again between the clicks.
    Row { list: ListId, index: usize, id: u64 },
    /// The fold marker or head of a transcript block: a click toggles it.
    Fold { block: String },
    /// A button or a key chip: a press is the same as pressing this key.
    Key(KeyCode),
    /// A link or another opener the screen resolves by its number.
    Link(u16),
    /// Text a user may select, which also scrolls.
    Text(PaneId),
    /// A pane that only scrolls.
    Scroll(PaneId),
    /// Takes the click and the wheel and does nothing with them: a popup or a confirm line over what lies under it.
    Block,
}

/// One rectangle of a draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub rect: Rect,
    pub target: Target,
    /// The key that does what a click here does (for the footer, the help and a test).
    pub key: &'static str,
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    u32::from(x) >= u32::from(r.x)
        && u32::from(x) < u32::from(r.x) + u32::from(r.width)
        && u32::from(y) >= u32::from(r.y)
        && u32::from(y) < u32::from(r.y) + u32::from(r.height)
}

/// The regions of the last draw.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    frame: Rect,
    regions: Vec<Region>,
    stale: bool,
    epoch: u64,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a draw: forgets every region and records the size of the frame the next regions are clipped to.
    pub fn begin(&mut self, frame: Rect) {
        self.frame = frame;
        self.regions.clear();
        self.stale = false;
    }

    /// Records a region. It is clipped to the frame; an empty one is dropped. A later region wins over an earlier one.
    pub fn push(&mut self, rect: Rect, target: Target, key: &'static str) {
        let rect = rect.intersection(self.frame);
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.regions.push(Region { rect, target, key });
    }

    /// The terminal changed size: nothing in the map is true until the next [`HitMap::begin`].
    pub fn resized(&mut self) {
        self.stale = true;
        self.epoch += 1;
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Counts the resizes the map has seen; a gesture or a double click that began in an earlier epoch is over.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// The regions that name no key. Every click has a key, so a test asserts this is empty.
    pub fn unkeyed(&self) -> Vec<&Region> {
        self.regions.iter().filter(|r| r.key.is_empty()).collect()
    }

    /// The region a press at `(x, y)` lands on: the last one drawn that holds the point. A stale map names none.
    pub fn target_at(&self, x: u16, y: u16) -> Option<&Region> {
        if self.stale {
            return None;
        }
        self.regions.iter().rev().find(|r| inside(r.rect, x, y))
    }

    /// The pane the wheel at `(x, y)` scrolls: the first text or scroll region beneath the point, looking through rows, folds and chips.
    /// A [`Target::Block`] on the way stops the search.
    pub fn pane_at(&self, x: u16, y: u16) -> Option<&Region> {
        match self.under(x, y, false) {
            Under::Found(r) => Some(r),
            _ => None,
        }
    }

    /// The text pane beneath `(x, y)`, looking through everything but a [`Target::Block`].
    pub fn text_at(&self, x: u16, y: u16) -> Option<&Region> {
        match self.under(x, y, true) {
            Under::Found(r) => Some(r),
            _ => None,
        }
    }

    /// Where the text pane `pane` is in this draw (the last region that names it), or `None` when the map is stale or the screen has no such
    /// pane. A held drag asks for it again at every repeat, so the edge it scrolls past is the pane as drawn now.
    pub fn pane_rect(&self, pane: PaneId) -> Option<Rect> {
        if self.stale {
            return None;
        }
        self.regions.iter().rev().find_map(|r| match r.target {
            Target::Text(p) if p == pane => Some(r.rect),
            _ => None,
        })
    }

    fn under(&self, x: u16, y: u16, text_only: bool) -> Under<'_> {
        if self.stale {
            return Under::Nothing;
        }
        for r in self.regions.iter().rev().filter(|r| inside(r.rect, x, y)) {
            match &r.target {
                Target::Text(_) => return Under::Found(r),
                Target::Scroll(_) if !text_only => return Under::Found(r),
                Target::Block => return Under::Blocked,
                _ => {}
            }
        }
        Under::Nothing
    }
}

enum Under<'a> {
    Found(&'a Region),
    Blocked,
    Nothing,
}

/// A stable identity for a row from the text that names what it shows (a run id): FNV-1a.
pub fn stable_id(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// What the screen does about a pointer event. Coordinates are screen cells inside the pane they name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Focus(BoxId),
    Select {
        list: ListId,
        index: usize,
        id: u64,
    },
    Open {
        list: ListId,
        index: usize,
        id: u64,
    },
    ToggleFold {
        block: String,
    },
    /// Do what pressing the key does.
    Press(KeyCode),
    OpenLink(u16),
    /// Scroll a pane by `lines` (positive: toward newer text). `pane` is `None` when no pane lies under the pointer: the screen's own
    /// default scroll applies.
    Wheel {
        pane: Option<PaneId>,
        lines: i32,
    },
    SelectionStart {
        pane: PaneId,
        x: u16,
        y: u16,
    },
    SelectionExtend {
        pane: PaneId,
        x: u16,
        y: u16,
    },
    /// The button came up: the selection is made (the screen copies it now).
    SelectionEnd {
        pane: PaneId,
        x: u16,
        y: u16,
    },
    /// A drag holds outside the pane: scroll it.
    Autoscroll {
        pane: PaneId,
        lines: i32,
    },
    /// Drop the selection (or the half-made one).
    ClearSelection,
}

/// How far a wheel notch goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WheelSettings {
    /// Lines per notch, 1 to [`MAX_WHEEL_STEP`].
    pub step: u16,
    /// A fast run of notches in one direction speeds up.
    pub accelerate: bool,
}

impl WheelSettings {
    /// A step clamped to its range, with acceleration on.
    pub fn new(step: u16) -> Self {
        Self {
            step: step.clamp(1, MAX_WHEEL_STEP),
            accelerate: true,
        }
    }

    /// The step a user wrote in the config: digits only, 1 to [`MAX_WHEEL_STEP`].
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: u16 = t.parse().ok()?;
        (1..=MAX_WHEEL_STEP).contains(&n).then(|| Self::new(n))
    }
}

impl Default for WheelSettings {
    fn default() -> Self {
        Self::new(DEFAULT_WHEEL_STEP)
    }
}

#[derive(Clone, Debug)]
struct WheelState {
    settings: WheelSettings,
    last: Option<(Instant, bool)>,
    streak: u32,
}

impl WheelState {
    /// Lines for one notch: positive down. The step grows by one step for each [`ACCEL_EVERY`] notches of a fast run, to [`ACCEL_MAX`].
    fn notch(&mut self, down: bool, now: Instant) -> i32 {
        let fast = self.settings.accelerate
            && self.last.is_some_and(|(t, d)| {
                d == down && now.checked_duration_since(t).is_some_and(|g| g < ACCEL_GAP)
            });
        self.streak = if fast {
            self.streak.saturating_add(1)
        } else {
            0
        };
        self.last = Some((now, down));
        let factor = 1 + (self.streak / ACCEL_EVERY).min(ACCEL_MAX - 1);
        let lines = i32::from(self.settings.step) * i32::try_from(factor).unwrap_or(1);
        if down { lines } else { -lines }
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Fold(String),
    Text,
}

/// A left press that waits for the button to come up.
#[derive(Clone, Debug)]
struct Gesture {
    kind: Kind,
    origin: (u16, u16),
    pane: Option<(PaneId, Rect)>,
    /// A selection was started (the pointer left the cell it pressed).
    moved: bool,
    last: (u16, u16),
    last_autoscroll: Option<Instant>,
    /// The autoscroll repeat is on. A key turns it off for the rest of the gesture; the next drag report turns it on again.
    repeat: bool,
}

#[derive(Clone, Debug)]
struct Chain {
    list: ListId,
    id: u64,
    at: Instant,
}

/// The pointer: gestures in flight, the double-click chain, the wheel and whether text is selected.
#[derive(Clone, Debug)]
pub struct Pointer {
    wheel: WheelState,
    selected: bool,
    gesture: Option<Gesture>,
    chain: Option<Chain>,
    epoch: u64,
}

fn clamp_into(r: Rect, x: u16, y: u16) -> (u16, u16) {
    (
        x.clamp(r.x, r.x + r.width - 1),
        y.clamp(r.y, r.y + r.height - 1),
    )
}

impl Pointer {
    pub fn new(wheel: WheelSettings) -> Self {
        Self {
            wheel: WheelState {
                settings: wheel,
                last: None,
                streak: 0,
            },
            selected: false,
            gesture: None,
            chain: None,
            epoch: 0,
        }
    }

    /// Whether a text selection exists (made by a drag that came up, not yet cleared).
    pub fn has_selection(&self) -> bool {
        self.selected
    }

    /// The screen dropped the selection by itself (a key, or a layout change); the next click works as usual.
    pub fn clear_selection(&mut self) {
        self.selected = false;
    }

    /// A key was pressed: two clicks with a key between them are not a double click, and a drag that is held outside its pane stops
    /// scrolling on the clock (the next drag report, a pointer that moves, starts it again).
    pub fn note_key(&mut self) {
        self.chain = None;
        if let Some(g) = self.gesture.as_mut() {
            g.repeat = false;
        }
    }

    /// Forgets everything in flight (the terminal lost focus, the screen changed).
    pub fn reset(&mut self) {
        self.gesture = None;
        self.chain = None;
        self.selected = false;
        self.wheel.last = None;
        self.wheel.streak = 0;
    }

    /// A resize since the last event ends every gesture, the double click and the selection; says so once.
    fn sync(&mut self, map: &HitMap) -> Vec<Action> {
        if map.epoch() == self.epoch {
            return Vec::new();
        }
        self.epoch = map.epoch();
        self.chain = None;
        let half_made = self.gesture.take().is_some_and(|g| g.moved);
        let had = self.selected;
        self.selected = false;
        if half_made || had {
            vec![Action::ClearSelection]
        } else {
            Vec::new()
        }
    }

    /// Turns one mouse event into actions.
    pub fn handle(&mut self, map: &HitMap, ev: MouseEvent, now: Instant) -> Vec<Action> {
        let mut out = self.sync(map);
        let (x, y) = (ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(map, x, y, now, &mut out),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(map, x, y, now, &mut out),
            MouseEventKind::Up(MouseButton::Left) => self.release(map, x, y, &mut out),
            MouseEventKind::ScrollDown => self.scroll(map, x, y, true, now, &mut out),
            MouseEventKind::ScrollUp => self.scroll(map, x, y, false, now, &mut out),
            _ => {}
        }
        out
    }

    /// A drag that is held outside its text pane and would scroll it now: the pane, its rectangle in this draw, the cell the pointer holds
    /// and the lines to scroll (negative above the pane, positive below). `None` when nothing is held, the drag has not left its first cell, a
    /// key stopped the repeat, the pointer is inside the pane, the screen was resized since the press (or is waiting to be drawn again) or
    /// the screen has no such pane any more. [`Pointer::tick`] and [`Pointer::autoscroll_due`] both ask this, so they cannot disagree.
    fn held_outside(&self, map: &HitMap) -> Option<(PaneId, Rect, (u16, u16), i32)> {
        if map.is_stale() || map.epoch() != self.epoch {
            return None;
        }
        let g = self.gesture.as_ref()?;
        if !g.moved || !g.repeat {
            return None;
        }
        let (pane, _) = g.pane?;
        let rect = map.pane_rect(pane)?;
        let lines = edge_lines(rect, g.last.1, i32::from(self.wheel.settings.step))?;
        Some((pane, rect, g.last, lines))
    }

    /// Repeats the autoscroll of a drag that holds outside its pane (terminals report a drag only when the pointer moves). Call it
    /// when [`Pointer::autoscroll_due`] says it is time: the scroll comes once per [`AUTOSCROLL_EVERY`], also when the call is late.
    pub fn tick(&mut self, map: &HitMap, now: Instant) -> Vec<Action> {
        let mut out = self.sync(map);
        let Some((pane, rect, (x, y), lines)) = self.held_outside(map) else {
            return out;
        };
        let Some(g) = self.gesture.as_mut() else {
            return out;
        };
        let due = g.last_autoscroll.is_none_or(|t| {
            now.checked_duration_since(t)
                .is_some_and(|d| d >= AUTOSCROLL_EVERY)
        });
        if due {
            g.last_autoscroll = Some(now);
            let (cx, cy) = clamp_into(rect, x, y);
            out.push(Action::Autoscroll { pane, lines });
            out.push(Action::SelectionExtend { pane, x: cx, y: cy });
        }
        out
    }

    /// When [`Pointer::tick`] next scrolls: `Some` while a drag is held outside its pane (the instant of the last scroll plus
    /// [`AUTOSCROLL_EVERY`], or `now` when it never scrolled), `None` otherwise. The event loop sleeps until then only while this is `Some`,
    /// so a screen with no drag held has no timer but the one-second poll. A deadline that has passed means `tick` scrolls.
    pub fn autoscroll_due(&self, map: &HitMap, now: Instant) -> Option<Instant> {
        self.held_outside(map)?;
        let g = self.gesture.as_ref()?;
        Some(
            g.last_autoscroll
                .map_or(now, |t| t.checked_add(AUTOSCROLL_EVERY).unwrap_or(now)),
        )
    }

    fn press(&mut self, map: &HitMap, x: u16, y: u16, now: Instant, out: &mut Vec<Action>) {
        // A press whose button never came up: the half-made selection goes.
        if self.gesture.take().is_some_and(|g| g.moved) {
            out.push(Action::ClearSelection);
        }
        if map.is_stale() {
            self.chain = None;
            return;
        }
        let hit = map.target_at(x, y);
        if self.selected {
            self.selected = false;
            self.chain = None;
            out.push(Action::ClearSelection);
            if let Some(Region {
                target: Target::Text(pane),
                rect,
                ..
            }) = hit
            {
                self.gesture = Some(Gesture::text(*pane, *rect, x, y));
            }
            return;
        }
        let Some(region) = hit else {
            self.chain = None;
            return;
        };
        match &region.target {
            Target::Row { list, index, id } => {
                let again = self.chain.as_ref().is_some_and(|c| {
                    c.list == *list
                        && c.id == *id
                        && now
                            .checked_duration_since(c.at)
                            .is_some_and(|d| d <= DOUBLE_CLICK)
                });
                if again {
                    self.chain = None;
                    out.push(Action::Open {
                        list: *list,
                        index: *index,
                        id: *id,
                    });
                } else {
                    self.chain = Some(Chain {
                        list: *list,
                        id: *id,
                        at: now,
                    });
                    out.push(Action::Select {
                        list: *list,
                        index: *index,
                        id: *id,
                    });
                }
            }
            Target::Box(b) => {
                self.chain = None;
                out.push(Action::Focus(*b));
            }
            Target::Key(k) => {
                self.chain = None;
                out.push(Action::Press(*k));
            }
            Target::Link(n) => {
                self.chain = None;
                out.push(Action::OpenLink(*n));
            }
            Target::Fold { block } => {
                self.chain = None;
                let pane = map.text_at(x, y).and_then(|r| match r.target {
                    Target::Text(p) => Some((p, r.rect)),
                    _ => None,
                });
                self.gesture = Some(Gesture {
                    kind: Kind::Fold(block.clone()),
                    origin: (x, y),
                    pane,
                    moved: false,
                    last: (x, y),
                    last_autoscroll: None,
                    repeat: true,
                });
            }
            Target::Text(pane) => {
                self.chain = None;
                self.gesture = Some(Gesture::text(*pane, region.rect, x, y));
            }
            Target::Scroll(_) | Target::Block => self.chain = None,
        }
    }

    fn drag(&mut self, map: &HitMap, x: u16, y: u16, now: Instant, out: &mut Vec<Action>) {
        let step = i32::from(self.wheel.settings.step);
        let Some(g) = self.gesture.as_mut() else {
            return;
        };
        if map.is_stale() {
            return;
        }
        g.last = (x, y);
        // The pointer moved: a key that stopped the repeat does not hold any more.
        g.repeat = true;
        let Some((pane, rect)) = g.pane else {
            return;
        };
        if !g.moved {
            if (x, y) == g.origin {
                return;
            }
            g.moved = true;
            out.push(Action::SelectionStart {
                pane,
                x: g.origin.0,
                y: g.origin.1,
            });
        }
        let (cx, cy) = clamp_into(rect, x, y);
        out.push(Action::SelectionExtend { pane, x: cx, y: cy });
        if let Some(lines) = edge_lines(rect, y, step) {
            g.last_autoscroll = Some(now);
            out.push(Action::Autoscroll { pane, lines });
        }
    }

    fn release(&mut self, map: &HitMap, x: u16, y: u16, out: &mut Vec<Action>) {
        let Some(g) = self.gesture.take() else {
            return;
        };
        if map.is_stale() {
            if g.moved {
                out.push(Action::ClearSelection);
            }
            return;
        }
        let same_cell = (x, y) == g.origin;
        match (g.moved, same_cell) {
            // A drag that ended somewhere else: the selection is made.
            (true, false) => {
                if let Some((pane, rect)) = g.pane {
                    let (cx, cy) = clamp_into(rect, x, y);
                    out.push(Action::SelectionEnd { pane, x: cx, y: cy });
                    self.selected = true;
                }
            }
            // The button came up elsewhere and no drag was reported: still a selection.
            (false, false) => {
                if let Some((pane, rect)) = g.pane {
                    let (cx, cy) = clamp_into(rect, x, y);
                    out.push(Action::SelectionStart {
                        pane,
                        x: g.origin.0,
                        y: g.origin.1,
                    });
                    out.push(Action::SelectionExtend { pane, x: cx, y: cy });
                    out.push(Action::SelectionEnd { pane, x: cx, y: cy });
                    self.selected = true;
                }
            }
            // The pointer wandered and came back, or never moved: a click.
            (moved, true) => {
                if moved {
                    out.push(Action::ClearSelection);
                }
                if let Kind::Fold(block) = g.kind
                    && matches!(
                        map.target_at(x, y).map(|r| &r.target),
                        Some(Target::Fold { block: now }) if *now == block
                    )
                {
                    out.push(Action::ToggleFold { block });
                }
            }
        }
    }

    fn scroll(
        &mut self,
        map: &HitMap,
        x: u16,
        y: u16,
        down: bool,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        self.chain = None;
        let pane = match map.under(x, y, false) {
            Under::Blocked => return,
            Under::Found(r) => match r.target {
                Target::Text(p) | Target::Scroll(p) => Some(p),
                _ => None,
            },
            Under::Nothing => None,
        };
        let lines = self.wheel.notch(down, now);
        out.push(Action::Wheel { pane, lines });
    }
}

impl Gesture {
    fn text(pane: PaneId, rect: Rect, x: u16, y: u16) -> Self {
        Self {
            kind: Kind::Text,
            origin: (x, y),
            pane: Some((pane, rect)),
            moved: false,
            last: (x, y),
            last_autoscroll: None,
            repeat: true,
        }
    }
}

/// Lines to scroll when the pointer at row `y` is above or below `rect`: negative above, positive below.
fn edge_lines(rect: Rect, y: u16, step: i32) -> Option<i32> {
    if y < rect.y {
        Some(-step)
    } else if u32::from(y) >= u32::from(rect.y) + u32::from(rect.height) {
        Some(step)
    } else {
        None
    }
}
