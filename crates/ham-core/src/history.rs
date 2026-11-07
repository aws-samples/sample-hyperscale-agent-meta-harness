//! The prompt history of the chat box. Up at the first row and Down at the last row step through earlier prompts, and Ctrl-R
//! searches them. The ring holds, newest first, this session's prompts, then the run's earlier user messages (the screen takes them from
//! the stored conversation), then the prompts of earlier runs (the screen takes them from the run index: the first 2,000 characters of each,
//! and the full text is asked for when one of them is recalled). The same complete text is never listed twice.
//!
//! Everything lives in memory for the life of the process. The ring has no way to reach the disk, and a test reads this source to prove it
//! (invariant 5: the client keeps no history of its own).
//!
//! Walking is over a snapshot taken at the first Up, so a conversation that grows meanwhile does not shift the entry on screen; the
//! first Up skips entries equal to what the box already holds. Down past the newest entry gives the draft back, with the taint it had
//! when the walk began.
//!
//! A prompt of an earlier run is only a short form until the run's detail has been fetched: its first 2,000 characters
//! ([`Earlier::from_prompt`]), or, from the Runs list, its title ([`Earlier::from_title`]: the first line, cut at 80 characters). While
//! that is pending ([`HistoryRing::pending_full`]) the screen must not send the box; [`HistoryRing::supply_full`] hands back the full text
//! with the short form it replaces, and the screen replaces the box only if it still holds that text. A prompt fetched once stays in full
//! when the list is read again ([`HistoryRing::set_earlier_runs`]). Text from the server is cleaned of escape sequences and controls.
//!
//! [`ReverseSearch::shown`] gives the matching entry as the one row the screen draws under the Ctrl-R bar, with the character ranges of
//! the query's occurrences in that row.

use crate::run_index::{prompt_head, title_of};
use crate::sanitize::{display_line, sanitize, sanitize_line};
use crate::search::{find_ranges, fit_end};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// The most prompts of this session that the ring keeps; the oldest go first.
pub const SESSION_MAX: usize = 500;
/// The most bytes of this session's prompts that the ring keeps; the oldest go first, and the newest prompt is kept whatever its size.
pub const SESSION_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The characters of an earlier run's prompt that the run index carries.
pub const EARLIER_PROMPT_CHARS: usize = 2_000;

/// What a short form of a prompt is, which decides how the full prompt is recognised when it arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Short {
    /// The first characters of the prompt (or the whole prompt): the full text begins with it.
    Head,
    /// The title the run index makes of the prompt ([`Earlier::from_title`]): the full text has that title, by the server's rule.
    Title,
}

/// The prompt of an earlier run as the run index holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Earlier {
    pub run_id: String,
    /// At most [`EARLIER_PROMPT_CHARS`] characters.
    pub text: String,
    /// The prompt is longer than `text`: the full text is fetched with the run's detail when it is recalled.
    pub truncated: bool,
    /// How `text` stands for the prompt.
    pub short: Short,
}

impl Earlier {
    /// The index's copy of `prompt`: cleaned of escape sequences and controls, cut at [`EARLIER_PROMPT_CHARS`] characters, with the flag
    /// set when something was cut.
    pub fn from_prompt(run_id: &str, prompt: &str) -> Earlier {
        let prompt = sanitize(prompt);
        let text: String = prompt.chars().take(EARLIER_PROMPT_CHARS).collect();
        let truncated = text.len() < prompt.len();
        Earlier {
            run_id: run_id.to_string(),
            text,
            truncated,
            short: Short::Head,
        }
    }
}

/// The characters that show a line break of an entry in the one-row match: a space, the return symbol, a space.
const LINE_BREAK: &str = " \u{23ce} ";

impl Earlier {
    /// The index's copy of a run's title (the Runs list sends `title`: the first non-blank line of the prompt, at most 80 characters, cut with
    /// an ellipsis), cleaned of escape sequences and controls. A title is always a short form, because the prompt may go on: the flag is set
    /// and the full text is fetched with the run's detail when the entry is recalled.
    pub fn from_title(run_id: &str, title: &str) -> Earlier {
        Earlier {
            run_id: run_id.to_string(),
            text: sanitize_line(title).trim().to_string(),
            truncated: true,
            short: Short::Title,
        }
    }
}

/// Whether a text is the prompt that the short form `text` stands for. A [`Short::Head`] begins the prompt (`clean`, the prompt without
/// escape sequences and controls). A [`Short::Title`] is the title the run index makes of it (`run_index::title_of` over the first
/// 2,000 characters: the first non-blank line with its blanks collapsed, cut at 80 characters with an ellipsis, so it is not always a
/// prefix); the server makes it from the prompt as it was written, so the check looks at `raw` too, because a control character in the first
/// line is a space in the title and absent from `clean`. A title is not taken for a prefix: `Fix` is not the title of `Fix the wrong repo`.
fn is_short_of(text: &str, short: Short, raw: &str, clean: &str) -> bool {
    match short {
        Short::Head => clean.starts_with(text),
        Short::Title => [raw, clean]
            .iter()
            .any(|t| title_of(&prompt_head(t).0) == text),
    }
}

/// The matching entry of a Ctrl-R search as one row of text, and where the query is in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The entry as the screen draws it: each line break shown as the three characters of `LINE_BREAK` (a space, the return symbol, a space),
    /// controls and escape sequences gone (`display_line`).
    pub text: String,
    /// The occurrences of the query in `text`, as ranges of characters: the same occurrences [`ReverseSearch::highlights`] finds in the entry
    /// itself, mapped through the changes made to the text for the screen.
    pub marks: Vec<Range<usize>>,
}

/// The text of the box and whether it is tainted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    pub tainted: bool,
}

/// A text the box should now hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recall {
    pub text: String,
    /// The run whose full prompt must be fetched: `text` is the first 2,000 characters of it. [`HistoryRing::supply_full`] takes the answer.
    pub fetch_full: Option<String>,
    /// True for a text from the history; for the draft coming back, the taint the draft had.
    pub tainted: bool,
    /// Set on the answer of [`HistoryRing::supply_full`]: the prefix this text replaces. The screen replaces the box only if it still
    /// holds exactly that.
    pub replaces: Option<String>,
}

#[derive(Clone, Debug)]
struct Entry {
    text: String,
    /// Set for a prompt of an earlier run.
    run_id: Option<String>,
    truncated: bool,
    short: Short,
}

impl Entry {
    fn recall(&self) -> Recall {
        Recall {
            text: self.text.clone(),
            fetch_full: self.truncated.then(|| self.run_id.clone()).flatten(),
            tainted: true,
            replaces: None,
        }
    }
}

/// A walk through the entries: the snapshot, the position and the draft the box held.
#[derive(Clone, Debug)]
struct Walk {
    entries: Vec<Entry>,
    /// The first entry shown (the ones before it equal the box and were skipped).
    start: usize,
    pos: usize,
    draft: Draft,
}

/// A recalled prefix whose full prompt has been asked for.
#[derive(Clone, Debug)]
struct Pending {
    run_id: String,
    prefix: String,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryRing {
    /// This session's prompts, oldest first.
    session: Vec<String>,
    /// The run's earlier user messages, oldest first.
    run: Vec<String>,
    /// Prompts of earlier runs, newest first.
    earlier: Vec<Earlier>,
    walk: Option<Walk>,
    pending: Option<Pending>,
}

impl HistoryRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// A prompt that was sent: the newest entry. Trailing blanks are cut; a blank prompt and a repeat of the newest are ignored. Ends any
    /// walk and any wait for a full prompt. The oldest prompts go when there are more than [`SESSION_MAX`] or more than
    /// [`SESSION_MAX_BYTES`] bytes.
    pub fn record(&mut self, text: &str) {
        self.walk = None;
        self.pending = None;
        let t = text.trim_end();
        if t.trim().is_empty() || self.session.last().is_some_and(|l| l == t) {
            return;
        }
        self.session.push(t.to_string());
        if self.session.len() > SESSION_MAX {
            let extra = self.session.len() - SESSION_MAX;
            self.session.drain(..extra);
        }
        let mut total: usize = self.session.iter().map(String::len).sum();
        while total > SESSION_MAX_BYTES && self.session.len() > 1 {
            total -= self.session.remove(0).len();
        }
    }

    /// The user messages of the run on screen, oldest first (the order of the conversation), cleaned of escape sequences and controls.
    pub fn set_run_messages(&mut self, messages: Vec<String>) {
        self.run = messages
            .into_iter()
            .map(|m| sanitize(&m).trim_end().to_string())
            .filter(|m| !m.trim().is_empty())
            .collect();
    }

    /// The prompts of the earlier runs, newest first (the order of the Runs list). A prompt this ring already holds in full stays in full when
    /// its run comes back as a short form again (the list is read every few seconds, and a prompt that was fetched is not fetched twice).
    pub fn set_earlier_runs(&mut self, runs: Vec<Earlier>) {
        let fetched: HashMap<&str, &str> = self
            .earlier
            .iter()
            .filter(|e| !e.truncated)
            .map(|e| (e.run_id.as_str(), e.text.as_str()))
            .collect();
        let next: Vec<Earlier> = runs
            .into_iter()
            .map(|mut e| {
                if e.truncated
                    && let Some(full) = fetched.get(e.run_id.as_str())
                    && is_short_of(&e.text, e.short, full, full)
                {
                    e.text = (*full).to_string();
                    e.truncated = false;
                    e.short = Short::Head;
                }
                e
            })
            .collect();
        self.earlier = next;
    }

    /// Every entry, newest first, each complete text once. A truncated prompt is never taken for another entry: two prompts can share
    /// their first 2,000 characters.
    fn merged(&self) -> Vec<Entry> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for t in self.session.iter().rev().chain(self.run.iter().rev()) {
            if seen.insert(t.as_str()) {
                out.push(Entry {
                    text: t.clone(),
                    run_id: None,
                    truncated: false,
                    short: Short::Head,
                });
            }
        }
        for e in &self.earlier {
            if e.truncated || seen.insert(e.text.as_str()) {
                out.push(Entry {
                    text: e.text.clone(),
                    run_id: Some(e.run_id.clone()),
                    truncated: e.truncated,
                    short: e.short,
                });
            }
        }
        out
    }

    pub fn is_browsing(&self) -> bool {
        self.walk.is_some()
    }

    /// Notes what the box now holds: a prefix whose full prompt is wanted, or nothing to wait for.
    fn note(&mut self, r: &Recall) {
        self.pending = r.fetch_full.as_ref().map(|run_id| Pending {
            run_id: run_id.clone(),
            prefix: r.text.clone(),
        });
    }

    /// Up: the next older entry. The first Up starts a walk from a snapshot, keeps `current` (the box's text) with its taint as the draft
    /// and skips entries equal to it. `None` when there is nothing older (the oldest stays on screen).
    pub fn prev(&mut self, current: &str, tainted: bool) -> Option<Recall> {
        if self.walk.is_none() {
            let entries = self.merged();
            let start = entries.iter().position(|e| e.text != current)?;
            let recall = entries[start].recall();
            self.walk = Some(Walk {
                entries,
                start,
                pos: start,
                draft: Draft {
                    text: current.to_string(),
                    tainted,
                },
            });
            self.note(&recall);
            return Some(recall);
        }
        let w = self.walk.as_mut()?;
        if w.pos + 1 >= w.entries.len() {
            return None;
        }
        w.pos += 1;
        let recall = w.entries[w.pos].recall();
        self.note(&recall);
        Some(recall)
    }

    /// Down: the next newer entry; below the newest, the draft comes back with the taint it had and the walk ends. `None` when no walk is on.
    #[allow(clippy::should_implement_trait)] // the Down key of the box, the partner of `prev`: not an iterator
    pub fn next(&mut self) -> Option<Recall> {
        let w = self.walk.as_mut()?;
        if w.pos > w.start {
            w.pos -= 1;
            let recall = w.entries[w.pos].recall();
            self.note(&recall);
            return Some(recall);
        }
        let draft = self.walk.take()?.draft;
        self.pending = None;
        Some(Recall {
            text: draft.text,
            fetch_full: None,
            tainted: draft.tainted,
            replaces: None,
        })
    }

    /// Ends a walk and returns the draft with its taint (Esc on a recalled text).
    pub fn cancel(&mut self) -> Option<Draft> {
        self.pending = None;
        self.walk.take().map(|w| w.draft)
    }

    /// The run whose full prompt the screen is waiting for: the box holds a 2,000-character prefix and must not be sent yet.
    pub fn pending_full(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.run_id.as_str())
    }

    /// The box was edited (or emptied): there is no prefix left to replace, and the wait is over.
    pub fn forget_pending(&mut self) {
        self.pending = None;
    }

    /// The full prompt of an earlier run arrived. A text that the short form the ring holds does not stand for (it neither begins with it nor
    /// has it as its title) is not that prompt and is refused. Otherwise the ring keeps it, and the result is the recall to show if the screen was waiting for it or has it on screen;
    /// the screen replaces the box with it only if the box still holds `replaces`.
    pub fn supply_full(&mut self, run_id: &str, full: &str) -> Option<Recall> {
        let (prefix, short) = self
            .earlier
            .iter()
            .find(|e| e.run_id == run_id)
            .map(|e| (e.text.clone(), e.short))?;
        let clean = sanitize(full);
        if !is_short_of(&prefix, short, full, &clean) {
            return None;
        }
        let full = clean;
        for e in self.earlier.iter_mut().filter(|e| e.run_id == run_id) {
            e.text = full.clone();
            e.truncated = false;
            e.short = Short::Head;
        }
        let mut on_screen = false;
        if let Some(w) = self.walk.as_mut() {
            for (i, e) in w
                .entries
                .iter_mut()
                .enumerate()
                .filter(|(_, e)| e.run_id.as_deref() == Some(run_id))
            {
                e.text = full.clone();
                e.truncated = false;
                e.short = Short::Head;
                on_screen |= i == w.pos;
            }
        }
        let waited = self.pending.as_ref().is_some_and(|p| p.run_id == run_id);
        if waited || on_screen {
            let replaces = self
                .pending
                .take()
                .filter(|p| p.run_id == run_id)
                .map_or(prefix, |p| p.prefix);
            return Some(Recall {
                text: full,
                fetch_full: None,
                tainted: true,
                replaces: Some(replaces),
            });
        }
        None
    }

    /// Ctrl-R over a snapshot of the entries, with `draft` (the box's text) and its taint to give back on cancel.
    pub fn search(&self, draft: &str, tainted: bool) -> ReverseSearch {
        ReverseSearch {
            entries: self.merged(),
            draft: Draft {
                text: draft.to_string(),
                tainted,
            },
            query: String::new(),
            hits: Vec::new(),
            at: 0,
        }
    }

    /// Enter on a Ctrl-R search: the match as a recall, and a wait for the full prompt if the match is a prefix. Ends a walk.
    pub fn accept_search(&mut self, search: ReverseSearch) -> Option<Recall> {
        let recall = search.current_entry().map(Entry::recall)?;
        self.walk = None;
        self.note(&recall);
        Some(recall)
    }
}

/// The Ctrl-R search: type to narrow, Ctrl-R again for an older match, Enter to take it, Esc to give the draft back.
#[derive(Clone, Debug)]
pub struct ReverseSearch {
    entries: Vec<Entry>,
    draft: Draft,
    query: String,
    /// Indices into `entries` of the matches, newest first.
    hits: Vec<usize>,
    at: usize,
}

impl ReverseSearch {
    fn rescan(&mut self) {
        self.at = 0;
        self.hits = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !find_ranges(&e.text, &self.query).is_empty())
                .map(|(i, _)| i)
                .collect()
        };
    }

    /// A typed character (controls are ignored); the search starts again at the newest match.
    pub fn type_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.query.push(c);
        self.rescan();
    }

    pub fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.rescan();
        }
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.rescan();
    }

    /// Ctrl-R again: the next older match. `false` when there is none.
    pub fn older(&mut self) -> bool {
        if self.at + 1 < self.hits.len() {
            self.at += 1;
            return true;
        }
        false
    }

    /// The next newer match. `false` when the newest is shown.
    pub fn newer(&mut self) -> bool {
        if self.at > 0 {
            self.at -= 1;
            return true;
        }
        false
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    fn current_entry(&self) -> Option<&Entry> {
        self.hits.get(self.at).and_then(|&i| self.entries.get(i))
    }

    /// The matching entry. The search holds the entries as they were when it began: a full prompt that arrives later is not in them.
    pub fn current(&self) -> Option<&str> {
        self.current_entry().map(|e| e.text.as_str())
    }

    /// `(n, of)`: the position of the match, from 1.
    pub fn position(&self) -> Option<(usize, usize)> {
        (!self.hits.is_empty()).then_some((self.at + 1, self.hits.len()))
    }

    /// The character ranges of the query in the matching entry.
    pub fn highlights(&self) -> Vec<Range<usize>> {
        self.current()
            .map(|t| find_ranges(t, &self.query))
            .unwrap_or_default()
    }

    /// The matching entry as one row, with the query's occurrences in it. `None` while there is no match (an empty or blank query, or no entry
    /// holds it). The row is the entry with each line break shown as three characters and with controls and escape sequences taken out, so
    /// the ranges are mapped: each stretch between occurrences, and each occurrence, is made into its row text on its own and the lengths
    /// are added.
    pub fn shown(&self) -> Option<Shown> {
        let entry = self.current()?;
        let chars: Vec<char> = entry.chars().collect();
        let row = |part: &[char]| -> String {
            let text: String = part.iter().collect();
            display_line(&text.replace('\n', LINE_BREAK))
        };
        let mut text = String::new();
        let mut len = 0usize;
        let mut marks = Vec::new();
        let mut at = 0usize;
        for r in find_ranges(entry, &self.query) {
            let (start, end) = (r.start.min(chars.len()), r.end.min(chars.len()));
            if start < at || start >= end {
                continue;
            }
            let before = row(&chars[at..start]);
            len += before.chars().count();
            text.push_str(&before);
            let marked = row(&chars[start..end]);
            let n = marked.chars().count();
            if n > 0 {
                marks.push(len..len + n);
            }
            len += n;
            text.push_str(&marked);
            at = end;
        }
        text.push_str(&row(&chars[at..]));
        Some(Shown { text, marks })
    }

    /// Esc: the draft the box held, with its taint.
    pub fn cancel(self) -> Draft {
        self.draft
    }

    /// `history search: test  1/4` in at most `width` characters (the end is kept when it is cut).
    pub fn bar_text(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let mut s = String::from("history search:");
        if self.query.is_empty() {
            return fit_end(&s, width);
        }
        s.push(' ');
        s.push_str(&self.query);
        match self.position() {
            Some((n, of)) => s.push_str(&format!("  {n}/{of}")),
            None => s.push_str("  no match"),
        }
        fit_end(&s, width)
    }
}
