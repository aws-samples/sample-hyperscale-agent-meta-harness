//! Search inside one chat: the matcher and the state machine behind the slash bar of the run chat view.
//!
//! * **Matcher.** [`find_ranges`] finds the non-overlapping, leftmost occurrences of a query in a text, ignoring case, and gives them as
//!   ranges of characters (Unicode scalar values) of the text. Case folding is `char::to_lowercase` plus the sharp s (`ß` is `ss`), the long
//!   s and the final sigma, so `STRASSE` finds `Straße` and `ΟΔΟΣ` finds `οδος`. Nothing is normalized: a letter with a combining accent is
//!   not the precomposed letter. The scan is a Knuth-Morris-Pratt pass over the folded characters, linear in the text however the query
//!   is built ([`find_ranges_counted`] reports the steps, so a test counts instead of timing). A query that is empty or only blanks finds
//!   nothing.
//! * **Hits.** [`search_blocks`] looks at what the transcript draws: each block's head line and body, as the widget lays them out
//!   (`display_line` and `display_text`). The role word, the progress-feed label and the chrome the app draws around the transcript are not
//!   text of the conversation and are not searched. A [`Hit`] names the block, the part, the range, and how many non-blank characters come
//!   before it in the block (head first). That count is how the widget finds the wrapped row and paints the cells: wrapping drops blanks and
//!   nothing else, so the n-th non-blank character is the same character in the text and in the rows. At most [`MAX_MATCHES`] hits are
//!   kept; [`ChatSearch::truncated`] says when there were more.
//! * **State machine.** [`ChatSearch`] holds the query, the hits and the current one. `open` starts a search from a [`ScrollMark`] (where the
//!   view was); typing jumps to the first match at or after the block on top of the view (the newest match when the view is at the bottom);
//!   Enter accepts, Esc while typing puts the view back ([`Effect::Restore`]), Esc after accepting only clears; `next` and `prev` walk the
//!   hits with a wrap. A longer query is searched only in the blocks that held the shorter one, when the chat has not changed
//!   ([`ChatSearch::blocks_scanned`] counts the blocks looked at). The module reads no events and draws nothing: `tui/chat_search.rs`
//!   joins it to the transcript.

use crate::chat::{Block, ChatModel};
use crate::sanitize::{display_line, display_text};
use std::collections::{HashSet, VecDeque};
use std::ops::Range;

/// The most hits kept; the count then reads `10000+`.
pub const MAX_MATCHES: usize = 10_000;
/// The longest query, in characters.
pub const MAX_QUERY_CHARS: usize = 256;

// ---- folding and the matcher --------------------------------------------------------------------------------------------------------

/// The folded form of `c` in `out` (up to three characters); returns how many.
fn fold_char(c: char, out: &mut [char; 3]) -> usize {
    match c {
        // The sharp s and its capital are "ss"; the long s is "s"; a final sigma is a sigma.
        '\u{df}' | '\u{1e9e}' => {
            out[0] = 's';
            out[1] = 's';
            2
        }
        '\u{17f}' => {
            out[0] = 's';
            1
        }
        '\u{3c2}' => {
            out[0] = '\u{3c3}';
            1
        }
        _ => {
            let mut n = 0;
            for l in c.to_lowercase() {
                if n < out.len() {
                    out[n] = l;
                    n += 1;
                }
            }
            n
        }
    }
}

/// `s` with case folded as the matcher folds it.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut buf = ['\0'; 3];
    for c in s.chars() {
        let n = fold_char(c, &mut buf);
        out.extend(&buf[..n]);
    }
    out
}

/// A folded query with its failure table (Knuth-Morris-Pratt).
#[derive(Clone, Debug)]
struct Needle {
    chars: Vec<char>,
    fail: Vec<usize>,
}

impl Needle {
    /// `None` for a query that is empty or only blanks. The query is cut at [`MAX_QUERY_CHARS`] characters.
    fn new(query: &str) -> Option<Needle> {
        if query.trim().is_empty() {
            return None;
        }
        let mut chars = Vec::new();
        let mut buf = ['\0'; 3];
        for c in query.chars().take(MAX_QUERY_CHARS) {
            let n = fold_char(c, &mut buf);
            chars.extend_from_slice(&buf[..n]);
        }
        let mut fail = vec![0usize; chars.len()];
        let mut k = 0;
        for i in 1..chars.len() {
            while k > 0 && chars[i] != chars[k] {
                k = fail[k - 1];
            }
            if chars[i] == chars[k] {
                k += 1;
            }
            fail[i] = k;
        }
        Some(Needle { chars, fail })
    }

    /// True when `self` starts with all of `other` (so a match of `self` holds a match of `other`).
    fn extends(&self, other: &Needle) -> bool {
        self.chars.starts_with(&other.chars)
    }
}

/// The matches of `needle` in `text`: non-overlapping, leftmost, as character ranges of `text`; at most `limit`. `steps` counts the
/// folded characters fed to the automaton and the fallbacks taken.
fn scan(text: &str, needle: &Needle, limit: usize, steps: &mut u64) -> Vec<Range<usize>> {
    let m = needle.chars.len();
    let mut out: Vec<Range<usize>> = Vec::new();
    if m == 0 || limit == 0 {
        return out;
    }
    // The character of `text` each of the last `m` folded characters came from.
    let mut origins: VecDeque<usize> = VecDeque::with_capacity(m);
    let mut state = 0usize;
    let mut buf = ['\0'; 3];
    for (i, c) in text.chars().enumerate() {
        let n = fold_char(c, &mut buf);
        for &f in &buf[..n] {
            *steps += 1;
            if origins.len() == m {
                origins.pop_front();
            }
            origins.push_back(i);
            while state > 0 && needle.chars[state] != f {
                state = needle.fail[state - 1];
                *steps += 1;
            }
            if needle.chars[state] == f {
                state += 1;
            }
            if state == m {
                let start = origins.front().copied().unwrap_or(i);
                out.push(start..i + 1);
                if out.len() >= limit {
                    return out;
                }
                // The match took the whole character: what is left of its folded form (the second "s" of a sharp s) is part of it, and
                // the next match starts at the next character.
                state = 0;
                origins.clear();
                break;
            }
        }
    }
    out
}

/// The non-overlapping, leftmost matches of `query` in `text`, ignoring case, as ranges of characters of `text`. Nothing for an empty or
/// blank query.
pub fn find_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    find_ranges_counted(text, query).0
}

/// [`find_ranges`] and the steps the scan took (at most about twice the folded text, whatever the query).
pub fn find_ranges_counted(text: &str, query: &str) -> (Vec<Range<usize>>, u64) {
    let mut steps = 0;
    let Some(needle) = Needle::new(query) else {
        return (Vec::new(), 0);
    };
    let found = scan(text, &needle, usize::MAX, &mut steps);
    (found, steps)
}

// ---- hits over a chat model ---------------------------------------------------------------------------------------------------------

/// Which text of a block a hit is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The one-line head (a tool call's title, a diff's path).
    Head,
    Body,
}

/// One match in the chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The index of the block in the model.
    pub block: usize,
    /// The block's id.
    pub id: String,
    pub part: Part,
    /// Characters of the part's drawn text (`display_line` of the head, `display_text` of the body).
    pub range: Range<usize>,
    /// Non-blank characters before the match in the block's text, the head first.
    pub nb_start: usize,
    /// Non-blank characters in the match.
    pub nb_len: usize,
}

fn nonblank(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// The hits of `needle` in one part's text: each with the non-blank characters before it (`base` more for the characters of the part
/// before this one) and the non-blank characters in it.
fn part_hits(
    text: &str,
    needle: &Needle,
    base: usize,
    limit: usize,
    steps: &mut u64,
) -> Vec<(Range<usize>, usize, usize)> {
    let ranges = scan(text, needle, limit, steps);
    let mut out = Vec::with_capacity(ranges.len());
    let mut chars = text.chars().enumerate().peekable();
    let mut nb = 0usize;
    for r in ranges {
        while let Some(&(i, c)) = chars.peek() {
            if i >= r.start {
                break;
            }
            if !c.is_whitespace() {
                nb += 1;
            }
            chars.next();
        }
        let start_nb = nb;
        let mut len = 0usize;
        while let Some(&(i, c)) = chars.peek() {
            if i >= r.end {
                break;
            }
            if !c.is_whitespace() {
                nb += 1;
                len += 1;
            }
            chars.next();
        }
        out.push((r, base + start_nb, len));
    }
    out
}

/// What a scan of the blocks found.
struct Scan {
    hits: Vec<Hit>,
    truncated: bool,
    /// Blocks whose text was looked at.
    scanned: u64,
    /// Ids of the blocks that hold a hit.
    holders: HashSet<String>,
}

/// Searches `model`'s blocks (only those in `only` when it is given) for `needle`.
fn scan_blocks(model: &ChatModel, needle: &Needle, only: Option<&HashSet<String>>) -> Scan {
    let mut s = Scan {
        hits: Vec::new(),
        truncated: false,
        scanned: 0,
        holders: HashSet::new(),
    };
    let mut steps = 0u64;
    for (index, b) in model.blocks.iter().enumerate() {
        if let Some(only) = only
            && !only.contains(&b.id)
        {
            continue;
        }
        s.scanned += 1;
        let head = display_line(&b.head);
        let body = display_text(&b.body);
        let head_nb = nonblank(&head);
        for (part, text, base) in [(Part::Head, &head, 0), (Part::Body, &body, head_nb)] {
            if text.is_empty() {
                continue;
            }
            let remaining = MAX_MATCHES - s.hits.len();
            let found = part_hits(text, needle, base, remaining + 1, &mut steps);
            for (n, (range, nb_start, nb_len)) in found.into_iter().enumerate() {
                if n >= remaining {
                    s.truncated = true;
                    return s;
                }
                s.holders.insert(b.id.clone());
                s.hits.push(Hit {
                    block: index,
                    id: b.id.clone(),
                    part,
                    range,
                    nb_start,
                    nb_len,
                });
            }
        }
    }
    s
}

/// Every hit of `query` in `model`, in block order (head before body), at most [`MAX_MATCHES`].
pub fn search_blocks(model: &ChatModel, query: &str) -> Vec<Hit> {
    match Needle::new(query) {
        Some(n) => scan_blocks(model, &n, None).hits,
        None => Vec::new(),
    }
}

/// A hash of what the blocks say (their ids and revisions), to tell whether a chat has changed between two scans.
fn fingerprint(blocks: &[Block]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ blocks.len() as u64;
    for b in blocks {
        for byte in b.id.bytes() {
            h = (h ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        h = (h ^ b.rev).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ---- the state machine --------------------------------------------------------------------------------------------------------------

/// Where the view was when the search opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScrollMark {
    /// The first visible line.
    pub top_line: usize,
    /// The view followed the end of the chat.
    pub following: bool,
    /// The selected block.
    pub selected: Option<String>,
    /// The index of the block on the first row of the view.
    pub anchor: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// The bar is open and takes keys.
    Editing,
    /// Accepted: the matches stay marked and `n` and `N` walk them.
    Active,
}

/// The keys of the search bar. The app maps its own keys (and the `n` and `N` of browse mode) to these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    /// Ctrl-W: the last word.
    DeleteWord,
    /// Ctrl-U: the whole query.
    Clear,
    Enter,
    Esc,
}

/// What the screen does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Bring this match into view (open its block first when the match is in a folded body).
    Show(Hit),
    /// Put the view back where it was.
    Restore(ScrollMark),
}

/// How the current hit is chosen after a scan.
#[derive(Clone, Copy)]
enum Pick {
    /// The first hit at or after the block on top of the view; the newest when the view follows the end.
    FromAnchor,
    /// The hit the search stood on, or the next one after its place.
    Keep,
}

/// What the last scan was for, to narrow the next one.
#[derive(Clone, Debug)]
struct Narrow {
    needle: Needle,
    /// Ids of the blocks that held a hit.
    holders: HashSet<String>,
    /// The hit list was cut at [`MAX_MATCHES`], so the holders are not all of them.
    cut: bool,
    /// The chat's fingerprint.
    print: u64,
}

#[derive(Clone, Debug)]
pub struct ChatSearch {
    phase: Phase,
    query: String,
    mark: ScrollMark,
    hits: Vec<Hit>,
    cur: Option<usize>,
    truncated: bool,
    wrapped: bool,
    scanned: u64,
    last: Option<Narrow>,
}

impl Default for ChatSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatSearch {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            query: String::new(),
            mark: ScrollMark::default(),
            hits: Vec::new(),
            cur: None,
            truncated: false,
            wrapped: false,
            scanned: 0,
            last: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The bar is open or matches are marked: Esc belongs to the search before anything else. It is true while typing too, so it is not
    /// the test for `n` and `N`: use [`walks_matches`](Self::walks_matches) for them and [`takes_text`](Self::takes_text) for characters.
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// The bar is open and every character typed is the query's (an `n` too).
    pub fn takes_text(&self) -> bool {
        self.phase == Phase::Editing
    }

    /// The search was accepted: `n` and `N` walk the matches.
    pub fn walks_matches(&self) -> bool {
        self.phase == Phase::Active
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    /// The index in [`hits`](Self::hits) of the current match.
    pub fn current(&self) -> Option<usize> {
        self.cur
    }

    /// There were more than [`MAX_MATCHES`] hits.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The last `next` or `prev` went past the end and came round.
    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// Blocks whose text the last scan looked at.
    pub fn blocks_scanned(&self) -> u64 {
        self.scanned
    }

    fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.query.clear();
        self.hits.clear();
        self.cur = None;
        self.truncated = false;
        self.wrapped = false;
        self.last = None;
    }

    /// Starts a search from the view described by `mark`: an empty query, the bar open.
    pub fn open(&mut self, mark: ScrollMark) {
        self.reset();
        self.phase = Phase::Editing;
        self.mark = mark;
    }

    /// Scans the model for the current query and picks the current hit.
    fn rescan(&mut self, model: &ChatModel, pick: Pick) {
        let before = self.cur.and_then(|i| self.hits.get(i)).cloned();
        let print = fingerprint(&model.blocks);
        let Some(needle) = Needle::new(&self.query) else {
            self.hits.clear();
            self.cur = None;
            self.truncated = false;
            self.scanned = 0;
            self.last = None;
            return;
        };
        let only = match &self.last {
            Some(n) if n.print == print && !n.cut && needle.extends(&n.needle) => Some(&n.holders),
            _ => None,
        };
        let found = scan_blocks(model, &needle, only);
        self.scanned = found.scanned;
        self.truncated = found.truncated;
        self.hits = found.hits;
        self.last = Some(Narrow {
            needle,
            holders: found.holders,
            cut: found.truncated,
            print,
        });
        self.cur = if self.hits.is_empty() {
            None
        } else {
            match pick {
                Pick::FromAnchor => Some(self.anchor_pick()),
                Pick::Keep => Some(self.keep_pick(before.as_ref())),
            }
        };
    }

    fn anchor_pick(&self) -> usize {
        if self.mark.following {
            return self.hits.len() - 1;
        }
        let anchor = self.mark.anchor.unwrap_or(0);
        self.hits
            .iter()
            .position(|h| h.block >= anchor)
            .unwrap_or(0)
    }

    fn keep_pick(&self, before: Option<&Hit>) -> usize {
        let Some(b) = before else {
            return self.anchor_pick();
        };
        if let Some(i) = self
            .hits
            .iter()
            .position(|h| h.id == b.id && h.part == b.part && h.range.start == b.range.start)
        {
            return i;
        }
        // Its hit is gone: the first hit at or after its place (block, then head before body, then position in the text).
        let place = |h: &Hit| (h.block, h.part == Part::Body, h.range.start);
        self.hits
            .iter()
            .position(|h| place(h) >= place(b))
            .unwrap_or(self.hits.len() - 1)
    }

    fn after_edit(&mut self, model: &ChatModel) -> Vec<Effect> {
        self.wrapped = false;
        self.rescan(model, Pick::FromAnchor);
        match self.cur.and_then(|i| self.hits.get(i)) {
            Some(h) => vec![Effect::Show(h.clone())],
            None => vec![Effect::Restore(self.mark.clone())],
        }
    }

    /// One key of the bar (while typing) or of the accepted search (Esc clears it).
    pub fn key(&mut self, key: Key, model: &ChatModel) -> Vec<Effect> {
        match self.phase {
            Phase::Idle => Vec::new(),
            Phase::Active => {
                if key == Key::Esc {
                    self.reset();
                }
                Vec::new()
            }
            Phase::Editing => match key {
                Key::Char(c) => {
                    if c.is_control() || self.query.chars().count() >= MAX_QUERY_CHARS {
                        return Vec::new();
                    }
                    self.query.push(c);
                    self.after_edit(model)
                }
                Key::Backspace => {
                    if self.query.pop().is_none() {
                        return Vec::new();
                    }
                    self.after_edit(model)
                }
                Key::DeleteWord => {
                    let kept = self
                        .query
                        .trim_end()
                        .trim_end_matches(|c: char| !c.is_whitespace())
                        .len();
                    if kept == self.query.len() {
                        return Vec::new();
                    }
                    self.query.truncate(kept);
                    self.after_edit(model)
                }
                Key::Clear => {
                    if self.query.is_empty() {
                        return Vec::new();
                    }
                    self.query.clear();
                    self.after_edit(model)
                }
                Key::Enter => {
                    if self.query.trim().is_empty() {
                        let mark = self.mark.clone();
                        self.reset();
                        return vec![Effect::Restore(mark)];
                    }
                    if !self.hits.is_empty() {
                        self.phase = Phase::Active;
                    }
                    Vec::new()
                }
                Key::Esc => {
                    let mark = self.mark.clone();
                    self.reset();
                    vec![Effect::Restore(mark)]
                }
            },
        }
    }

    /// Scans again when the chat is not the one the last scan saw.
    fn fresh(&mut self, model: &ChatModel) {
        let stale = self
            .last
            .as_ref()
            .is_none_or(|n| n.print != fingerprint(&model.blocks));
        if stale {
            self.rescan(model, Pick::Keep);
        }
    }

    /// `n`: the next match, coming round to the first after the last.
    pub fn next(&mut self, model: &ChatModel) -> Vec<Effect> {
        self.step(model, true)
    }

    /// `N`: the previous match, coming round to the last before the first.
    pub fn prev(&mut self, model: &ChatModel) -> Vec<Effect> {
        self.step(model, false)
    }

    fn step(&mut self, model: &ChatModel, forward: bool) -> Vec<Effect> {
        if self.phase == Phase::Idle {
            return Vec::new();
        }
        self.fresh(model);
        let n = self.hits.len();
        if n == 0 {
            return Vec::new();
        }
        let cur = self.cur.unwrap_or(if forward { n - 1 } else { 0 });
        let (to, wrapped) = if forward {
            ((cur + 1) % n, cur + 1 >= n)
        } else if cur == 0 {
            (n - 1, true)
        } else {
            (cur - 1, false)
        };
        self.cur = Some(to);
        self.wrapped = wrapped;
        vec![Effect::Show(self.hits[to].clone())]
    }

    /// The chat changed (a block arrived, one grew): the hits are searched again and the current one stays where it was. A chat that is
    /// the one the last scan saw (the same block ids and revisions) is not searched again.
    pub fn model_changed(&mut self, model: &ChatModel) {
        if self.phase == Phase::Idle || self.query.trim().is_empty() {
            return;
        }
        if self
            .last
            .as_ref()
            .is_some_and(|n| n.print == fingerprint(&model.blocks))
        {
            // The same blocks with the same revisions: the hits are what they were.
            self.scanned = 0;
            return;
        }
        self.rescan(model, Pick::Keep);
    }

    /// `3/17`, `no matches`, `1/10000+`; empty while the query is.
    pub fn count_text(&self) -> String {
        if self.query.trim().is_empty() {
            return String::new();
        }
        if self.hits.is_empty() {
            return "no matches".to_string();
        }
        let plus = if self.truncated { "+" } else { "" };
        match self.cur {
            Some(i) => format!("{}/{}{plus}", i + 1, self.hits.len()),
            None => format!("{}{plus}", self.hits.len()),
        }
    }

    /// The bar's text in at most `width` characters: `/query  3/17`, with ` wrapped` after a wrap. A narrow bar keeps the end of the
    /// query and the count; empty when no search is open.
    pub fn bar_text(&self, width: usize) -> String {
        if self.phase == Phase::Idle || width == 0 {
            return String::new();
        }
        let left = format!("/{}", self.query);
        let mut right = self.count_text();
        if !right.is_empty() && self.wrapped {
            right.push_str(" wrapped");
        }
        let left_len = left.chars().count();
        if right.is_empty() {
            return fit_end(&left, width);
        }
        let right_len = right.chars().count();
        if left_len + 2 + right_len <= width {
            return format!("{left}  {right}");
        }
        if width < right_len + 3 {
            return right.chars().take(width).collect();
        }
        format!("{}  {right}", fit_end(&left, width - right_len - 2))
    }
}

/// `text` in at most `width` characters, keeping the end and putting an ellipsis in place of the start that is cut.
pub(crate) fn fit_end(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n - (width - 1)).collect();
    format!("\u{2026}{tail}")
}
