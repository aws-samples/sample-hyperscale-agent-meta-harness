//! The large-paste placeholder of the chat box. A paste over [`PASTE_MAX_CHARS`] characters, or of [`PASTE_MIN_LINES`] lines or
//! more, goes into the box as `[Pasted n lines]` (a second one of the same size is `[Pasted n lines #2]`, and so on) and the text is
//! kept in a [`PasteStore`] on the side. A key expands the placeholder under the caret ([`PasteStore::expand_at`]); sending the draft,
//! opening it in an editor and recording it in the history use [`PasteStore::expand_all`], so the engine, the editor and the history
//! always see the text that was pasted.
//!
//! A paste is text, never keys: line breaks of every kind become `\n`, and escape sequences and control characters are removed before
//! anything is counted or shown (the same cleaning as [`crate::sanitize::sanitize`]). Pasted text taints the draft;
//! the box keeps that flag, this store only decides what is shown. Expanding is one pass: text that looks like a placeholder inside a
//! paste stays text, and a paste that holds the shape of a placeholder is always kept aside, so its text never sits in the box where a
//! send could read it as one. A token is never reused in the life of a store, even after [`PasteStore::clear`]: an undo that brings an
//! old token back finds plain text (and [`PasteStore::dangling`] says so before a send) instead of meeting a new paste that wears it.
//! The store lives with one draft in memory and is cleared when the draft is sent or emptied; nothing is stored on disk.

use crate::sanitize::sanitize;
use std::collections::HashSet;
use std::ops::Range;

/// A paste longer than this many characters becomes a placeholder.
pub const PASTE_MAX_CHARS: usize = 150;
/// A paste of this many lines or more becomes a placeholder.
pub const PASTE_MIN_LINES: usize = 3;

/// What goes into the box for a paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pasted {
    /// The cleaned text itself.
    Inline(String),
    /// The placeholder token.
    Placeholder(String),
}

/// A draft after one placeholder was expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded {
    pub text: String,
    /// The caret, in characters: right after the text that was put back.
    pub caret: usize,
}

/// The pastes of one draft.
#[derive(Clone, Debug, Default)]
pub struct PasteStore {
    /// `(token, text)` in the order the pastes were made.
    items: Vec<(String, String)>,
    /// Every token this store has made, live or not: none is made twice.
    issued: HashSet<String>,
}

/// Line breaks of every kind as `\n`, then escape sequences and controls removed.
fn clean(text: &str) -> String {
    sanitize(&text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// What every token starts with.
const TOKEN_OPEN: &str = "[Pasted ";

/// A token found in a text: bytes, characters, and the index of the paste it stands for.
struct Found {
    bytes: Range<usize>,
    chars: Range<usize>,
    item: usize,
}

impl PasteStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Forgets every paste (the draft was sent or emptied). Tokens already made are not made again.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// A paste: the cleaned text, or a placeholder when it is long.
    pub fn offer(&mut self, text: &str) -> Pasted {
        let text = clean(text);
        let lines = text.lines().count();
        let holds_token = text.contains(TOKEN_OPEN);
        if !holds_token && text.chars().count() <= PASTE_MAX_CHARS && lines < PASTE_MIN_LINES {
            return Pasted::Inline(text);
        }
        let lines = lines.max(1);
        let noun = if lines == 1 { "line" } else { "lines" };
        let base = format!("[Pasted {lines} {noun}");
        let mut token = format!("{base}]");
        let mut n = 2;
        while self.issued.contains(&token) {
            token = format!("{base} #{n}]");
            n += 1;
        }
        self.issued.insert(token.clone());
        self.items.push((token.clone(), text));
        Pasted::Placeholder(token)
    }

    /// The live placeholders of `text`, left to right, in one pass. No token is the start of another (each ends at its own `]`), so
    /// the first that matches at a place is the only one.
    fn find(&self, text: &str) -> Vec<Found> {
        let mut out = Vec::new();
        let mut char_at = 0usize;
        let mut chars = text.char_indices().peekable();
        while let Some(&(b, c)) = chars.peek() {
            let hit = (c == '[')
                .then(|| {
                    (0..self.items.len()).find(|&i| text[b..].starts_with(self.items[i].0.as_str()))
                })
                .flatten();
            match hit {
                Some(i) => {
                    let token = &self.items[i].0;
                    let n_chars = token.chars().count();
                    out.push(Found {
                        bytes: b..b + token.len(),
                        chars: char_at..char_at + n_chars,
                        item: i,
                    });
                    for _ in 0..n_chars {
                        chars.next();
                    }
                    char_at += n_chars;
                }
                None => {
                    chars.next();
                    char_at += 1;
                }
            }
        }
        out
    }

    /// `text` with every live placeholder replaced by the text that was pasted.
    pub fn expand_all(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for f in self.find(text) {
            out.push_str(&text[last..f.bytes.start]);
            out.push_str(&self.items[f.item].1);
            last = f.bytes.end;
        }
        out.push_str(&text[last..]);
        out
    }

    /// The character ranges of the live placeholders of `text`, for drawing them apart.
    pub fn placeholders_in(&self, text: &str) -> Vec<Range<usize>> {
        self.find(text).into_iter().map(|f| f.chars).collect()
    }

    /// Expands the placeholder the caret (a position in characters) is in or touches; where two touch it, the one before the caret.
    pub fn expand_at(&self, text: &str, caret: usize) -> Option<Expanded> {
        let f = self
            .find(text)
            .into_iter()
            .find(|f| f.chars.start <= caret && caret <= f.chars.end)?;
        let pasted = &self.items[f.item].1;
        let mut out = String::with_capacity(text.len() + pasted.len());
        out.push_str(&text[..f.bytes.start]);
        out.push_str(pasted);
        out.push_str(&text[f.bytes.end..]);
        Some(Expanded {
            text: out,
            caret: f.chars.start + pasted.chars().count(),
        })
    }

    /// Tokens of `text` that this store made and no longer holds (after [`clear`](Self::clear)): an undo brought one back, and a send
    /// would pass it on as plain text. The caller refuses the send and says so. Character ranges, left to right.
    pub fn dangling(&self, text: &str) -> Vec<Range<usize>> {
        let live: HashSet<&str> = self.items.iter().map(|(t, _)| t.as_str()).collect();
        let mut out: Vec<Range<usize>> = Vec::new();
        for token in self.issued.iter().filter(|t| !live.contains(t.as_str())) {
            for (b, _) in text.match_indices(token.as_str()) {
                let start = text[..b].chars().count();
                out.push(start..start + token.chars().count());
            }
        }
        out.sort_by_key(|r| r.start);
        out
    }

    /// The live token that ends exactly at `caret` (characters): a Backspace there takes the whole token.
    pub fn token_ending_at(&self, text: &str, caret: usize) -> Option<Range<usize>> {
        self.find(text)
            .into_iter()
            .find(|f| f.chars.end == caret)
            .map(|f| f.chars)
    }

    /// The live token that starts exactly at `caret`: a Delete there takes the whole token.
    pub fn token_starting_at(&self, text: &str, caret: usize) -> Option<Range<usize>> {
        self.find(text)
            .into_iter()
            .find(|f| f.chars.start == caret)
            .map(|f| f.chars)
    }
}
