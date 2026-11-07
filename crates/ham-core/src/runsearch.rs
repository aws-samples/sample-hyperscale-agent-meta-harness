//! Chat-content search: one bounded searchable body per run, written when the run is finalized, and
//! the matching `GET /v1/search?q=` reads, all pure so the Lambdas and the client share them.
//!
//! A run's chat is far larger than a DynamoDB read can afford (a Query reads up to 1 MB before it filters), so each run has one small
//! body item under its owner's partition: `PK = OWNER#<sha256 of the caller's ARN>`, `SK = CHAT#<createdAt>#<runId>`. It sits beside the
//! run index's item of the same run, not in it:
//! the run list reads the run index's items whole, a page of 100 at a time, so a 32 KiB body in each would make every run list read up to
//! 3.2 MB. Neither item carries an **`ownerPrincipalArn` attribute**, because the run-list index (GSI1) is sparse on exactly that
//! attribute and projects all of it: an item that carried it would enter the index with its body and shrink every list page. The
//! searchable body is at most 32 KiB ([`BODY_MAX_BYTES`]): the first 20 KiB of the conversation (the run's objective, the user's
//! answers, then the engine's messages, tool titles and file names in order) and the last 12 KiB, with a marker where the middle is left
//! out. So search covers what a person remembers (how it began and how it ended), says when it does not cover the rest ([`Coverage`]),
//! and is a bounded search of indexed content, not of the whole chat.
//!
//! What goes into a body: the objective and the user's answers (sources the engine cannot write), and from the stored transcript the
//! text of `agent_message_chunk` records (fragments of one message joined by `messageId`), the title and file paths of `tool_call`
//! and `tool_call_update` records. Never thoughts, the prompts agentd sent, the echo of the prompt (`user_message_chunk`), permission
//! records or anything else. The redaction function is passed to [`BodyBuilder::finish_with`], which applies it to each piece after the
//! fragments of one message are joined (a secret split across two fragments is whole by then) and fits the body to its limit afterwards,
//! because redaction can make text longer. `find_match_redacted` redacts the piece that holds a match before it clips the snippet.

use crate::transcript::Chunk;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// The largest body of one run.
pub const BODY_MAX_BYTES: usize = 32 * 1024;
/// The part of the body kept from the start of the conversation.
pub const HEAD_BYTES: usize = 20 * 1024;
/// The head's budget less the room the gap marker and its separator may need.
const HEAD_USABLE: usize = HEAD_BYTES - 8;
/// The part kept from the end.
pub const TAIL_BYTES: usize = BODY_MAX_BYTES - HEAD_BYTES;
/// The most of the run's objective that is kept.
pub const OBJECTIVE_MAX_BYTES: usize = 4 * 1024;
/// The most of one answer of the user's that is kept, and of all answers together.
pub const ANSWER_MAX_BYTES: usize = 1024;
pub const ANSWERS_MAX_BYTES: usize = 4 * 1024;
/// The most of one engine message or tool line that is kept.
pub const PIECE_MAX_BYTES: usize = 4 * 1024;
/// The most pieces whose place in the transcript is recorded.
pub const SEGS_MAX: usize = 1024;
/// The longest query, and the most terms it may have.
pub const MAX_Q_CHARS: usize = 200;
pub const MAX_TERMS: usize = 8;
/// About how many characters a snippet shows.
pub const SNIPPET_CHARS: usize = 120;
/// Runs one search examines (default and maximum). A search reaches back to the first run unless the request names a `since`; a long history
/// is read in pages of runs with a continuation token.
pub const DEFAULT_LIMIT: u32 = 200;
pub const MAX_LIMIT: u32 = 200;
/// The sort-key prefix of the run index's items, and the largest key that can follow it.
pub const SK_PREFIX: &str = crate::run_index::SEARCH_PREFIX;
pub const SK_CEILING: &str = "SRCH#~";
/// The sort-key prefix of the chat body items: below the run index's, so the run list's range never holds one.
pub const CHAT_PREFIX: &str = "CHAT#";
/// The marker between the head and the tail of a body when chunks lie between them.
pub const GAP_MARKER: &str = "\u{2026}";

/// `OWNER#<hex sha256 of the caller's principal ARN>` and the run index's sort key `SRCH#<createdAt>#<runId>`, as defined by the run
/// index (one definition of the partition and of the key the search reads its run list from).
pub use crate::run_index::{owner_pk, search_sk};

/// `CHAT#<createdAt>#<runId>`: the key of a run's chat body. Newest runs sort last, so a Query newest-first reads them first.
pub fn chat_sk(created_at: &str, run_id: &str) -> String {
    format!("{CHAT_PREFIX}{created_at}#{run_id}")
}

/// The lowest chat key of a window of runs that starts at creation time `oldest`.
pub fn chat_floor(oldest: &str) -> String {
    format!("{CHAT_PREFIX}{oldest}")
}

/// The highest chat key of a window of runs that ends at creation time `newest` (`~` sorts above every character of a run id).
pub fn chat_ceiling(newest: &str) -> String {
    format!("{CHAT_PREFIX}{newest}#~")
}

/// The lowest sort key a search from `since` (a date or a UTC time) reads.
pub fn sk_floor(since: &str) -> String {
    format!("{SK_PREFIX}{since}")
}

/// How much of a run's conversation the body holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Coverage {
    /// Everything that can be searched is in the body.
    Full,
    /// Some of the conversation is not in the body (the middle of a long run, an unreadable chunk, an unknown tail).
    Partial,
}

/// Where a piece of the body came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Objective,
    Answer,
    /// Record `rec` (its position in the chunk) of chunk `seq`.
    Engine {
        seq: u64,
        rec: u32,
    },
    Gap,
}

/// One searchable string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub source: Source,
    pub text: String,
    /// The `messageId` of an engine message fragment: adjacent fragments with the same id are one message.
    pub message: Option<String>,
}

/// Where a piece starts in the body, and the record it came from (`seq` is `None` for the user's own words and the marker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub start: u32,
    pub seq: Option<u64>,
    pub rec: u32,
}

/// The stored form of the places: `[[start, seq, rec], ...]` with `-1` for no seq.
pub fn segs_to_string(segs: &[Seg]) -> String {
    let rows: Vec<[i64; 3]> = segs
        .iter()
        .map(|s| {
            [
                i64::from(s.start),
                s.seq.map_or(-1, |q| i64::try_from(q).unwrap_or(i64::MAX)),
                i64::from(s.rec),
            ]
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
}

/// Reads [`segs_to_string`] back; anything unreadable is no places.
pub fn segs_from_str(s: &str) -> Vec<Seg> {
    let Ok(rows) = serde_json::from_str::<Vec<[i64; 3]>>(s) else {
        return Vec::new();
    };
    rows.into_iter()
        .take(SEGS_MAX)
        .filter_map(|[start, seq, rec]| {
            Some(Seg {
                start: u32::try_from(start).ok()?,
                seq: u64::try_from(seq).ok(),
                rec: u32::try_from(rec).ok()?,
            })
        })
        .collect()
}

/// A run's searchable body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchBody {
    pub body: String,
    pub segs: Vec<Seg>,
    /// The highest chunk number any piece came from.
    pub through: Option<u64>,
    pub coverage: Coverage,
}

/// What the search reads of one run: the run index's item (`SK` is its key; `promptHead`, `model`, `repository`) joined by `runId` to
/// the run's chat item (`body`, `segs`, `coverage`, `bodyThrough`, `indexedAt`), which a run without a transcript does not have. The
/// writer of the chat item sets only the body's attributes (an update of its own item), so it never touches the run index's item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchItem {
    #[serde(rename = "SK", default)]
    pub sk: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_through: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
}

// ---- building a body -------------------------------------------------------------------------------------------------

/// `text` cut to at most `max` bytes at a character boundary.
pub fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` keeping its last `max` bytes, cut at a character boundary.
pub fn clip_tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// [`clip`], but a cut that falls inside a word falls back to the last space when the kept text has one: a credential that straddles
/// the cut would otherwise keep its front half, which no redaction pattern recognizes.
pub fn clip_at_word(text: &str, max: usize) -> &str {
    let kept = clip(text, max);
    if kept.len() == text.len()
        || text[kept.len()..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        return kept;
    }
    match kept.rfind(char::is_whitespace) {
        Some(i) if i > 0 => &kept[..i],
        _ => kept,
    }
}

/// [`clip_tail`] with the same rule: the kept end does not start inside a word when it has a space to fall back on.
pub fn clip_tail_at_word(text: &str, max: usize) -> &str {
    let kept = clip_tail(text, max);
    let cut = text.len() - kept.len();
    if cut == 0
        || text[..cut]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        return kept;
    }
    match kept.find(char::is_whitespace) {
        Some(i) if !kept[i..].trim_start().is_empty() => kept[i..].trim_start(),
        _ => kept,
    }
}

fn normal_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of_content(data: &Value) -> Option<&str> {
    let content = data.get("content")?;
    match content.get("type").and_then(Value::as_str) {
        Some("text") | None => content.get("text").and_then(Value::as_str),
        Some(_) => None,
    }
}

fn path_words(data: &Value, title: &str, out: &mut Vec<String>) {
    let mut add = |p: &str| {
        if !p.is_empty() && !title.contains(p) && !out.iter().any(|w| w == p) {
            out.push(p.to_string());
        }
    };
    if let Some(locations) = data.get("locations").and_then(Value::as_array) {
        for loc in locations.iter().take(8) {
            if let Some(p) = loc.get("path").and_then(Value::as_str) {
                add(p);
            }
        }
    }
    if let Some(input) = data.get("rawInput") {
        for key in ["file_path", "filePath", "path"] {
            if let Some(p) = input.get(key).and_then(Value::as_str) {
                add(p);
            }
        }
    }
}

/// The searchable pieces of one chunk, in file order: each `agent_message_chunk` fragment, and one line per `tool_call` or
/// `tool_call_update` that names a title or a file. Nothing else is searchable, thoughts and prompts least of all.
pub fn pieces_of_chunk(chunk: &Chunk) -> Vec<Piece> {
    let seq = chunk.header.seq;
    let mut out = Vec::new();
    for (i, rec) in chunk.records.iter().enumerate() {
        let source = Source::Engine {
            seq,
            rec: u32::try_from(i).unwrap_or(u32::MAX),
        };
        // The outer kind and the payload's own update word must agree; a record that contradicts itself is not indexed.
        if let Some(inner) = rec.data.get("sessionUpdate").and_then(Value::as_str)
            && inner != rec.kind
        {
            continue;
        }
        match rec.kind.as_str() {
            "agent_message_chunk" => {
                if let Some(text) = text_of_content(&rec.data).filter(|t| !t.is_empty()) {
                    out.push(Piece {
                        source,
                        text: text.to_string(),
                        message: rec
                            .data
                            .get("messageId")
                            .and_then(Value::as_str)
                            .filter(|m| m.len() <= MESSAGE_ID_MAX_BYTES)
                            .map(str::to_string),
                    });
                }
            }
            "tool_call" | "tool_call_update" => {
                let title = rec.data.get("title").and_then(Value::as_str).unwrap_or("");
                let mut words = Vec::new();
                if !title.is_empty() {
                    words.push(title.to_string());
                }
                path_words(&rec.data, title, &mut words);
                if !words.is_empty() {
                    out.push(Piece {
                        source,
                        text: words.join(" "),
                        message: None,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// The longest `messageId` that is used to join fragments; a longer one is no id (a hostile id must not cost memory per piece).
pub const MESSAGE_ID_MAX_BYTES: usize = 128;
/// The most distinct tool lines remembered to leave out repeats.
const SEEN_MAX: usize = 8192;
/// The least room a head with a nearly full budget still fills with the start of one more piece.
const MIN_HEAD_CLIP: usize = 65;

/// A place in the transcript: chunk number and record position.
type Pos = (u64, u32);

fn pos_of(p: &Piece) -> Option<Pos> {
    match p.source {
        Source::Engine { seq, rec } => Some((seq, rec)),
        _ => None,
    }
}

fn line_key(text: &str) -> u64 {
    let d = Sha256::digest(text.to_lowercase().as_bytes());
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// Collects the pieces of one run into a body of at most [`BODY_MAX_BYTES`]. The caller feeds the user's words first, then the
/// newest chunks for the tail ([`BodyBuilder::tail_chunk`]) and the oldest for the head ([`BodyBuilder::head_chunk`]) until each says
/// it is full. When the tail stops inside a chunk ([`BodyBuilder::tail_cut_seq`]) the head is fed that chunk too and stops at the
/// tail's first piece, so every piece is in the body once.
#[derive(Debug, Default)]
pub struct BodyBuilder {
    trusted: Vec<Piece>,
    answers_bytes: usize,
    head: Vec<Piece>,
    head_bytes: usize,
    head_full: bool,
    head_reached_floor: bool,
    /// The first place the head left out (it was full, or kept only the start of the piece).
    head_refused_at: Option<Pos>,
    head_last_seq: Option<u64>,
    /// Newest first: the oldest piece, which new (older) fragments join, is the last.
    tail_rev: Vec<Piece>,
    tail_bytes: usize,
    tail_full: bool,
    /// The chunk the tail stopped inside.
    tail_cut: Option<u64>,
    tail_first: Option<Pos>,
    tail_first_seq: Option<u64>,
    seen: HashSet<u64>,
    /// Some text was cut to a size limit.
    clipped: bool,
}

impl BodyBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn trusted_bytes(&self) -> usize {
        self.trusted.iter().map(|p| p.text.len() + 1).sum()
    }

    /// The run's objective (first, at most [`OBJECTIVE_MAX_BYTES`]).
    pub fn add_objective(&mut self, text: &str) {
        let t = normal_text(text);
        if t.is_empty() {
            return;
        }
        if t.len() > OBJECTIVE_MAX_BYTES {
            self.clipped = true;
        }
        self.trusted.insert(
            0,
            Piece {
                source: Source::Objective,
                text: clip(&t, OBJECTIVE_MAX_BYTES).to_string(),
                message: None,
            },
        );
    }

    /// One answer the user gave (at most [`ANSWER_MAX_BYTES`] of it, [`ANSWERS_MAX_BYTES`] of all answers).
    pub fn add_answer(&mut self, text: &str) {
        let t = normal_text(text);
        if t.is_empty() {
            return;
        }
        let room = ANSWERS_MAX_BYTES.saturating_sub(self.answers_bytes);
        let keep = clip(&t, ANSWER_MAX_BYTES.min(room));
        if keep.len() < t.len() {
            self.clipped = true;
        }
        if keep.is_empty() {
            return;
        }
        self.answers_bytes += keep.len() + 1;
        self.trusted.push(Piece {
            source: Source::Answer,
            text: keep.to_string(),
            message: None,
        });
    }

    /// Whether a tool line repeats one already kept (the same file edited again adds nothing to search). Only lines that were kept
    /// are remembered, so a line the budget dropped is still welcome where there is room.
    fn repeats(&self, p: &Piece) -> bool {
        p.message.is_none()
            && matches!(p.source, Source::Engine { .. })
            && self.seen.contains(&line_key(&p.text))
    }

    fn remember(&mut self, p: &Piece) {
        if p.message.is_none()
            && matches!(p.source, Source::Engine { .. })
            && self.seen.len() < SEEN_MAX
        {
            self.seen.insert(line_key(&p.text));
        }
    }

    /// Adds the pieces of the next-oldest chunk to the head; returns whether the head takes no more. It stops at the tail's first
    /// piece when the tail is known.
    pub fn head_chunk(&mut self, seq: u64, pieces: Vec<Piece>) -> bool {
        for mut p in pieces {
            if self.head_reached_floor {
                break;
            }
            let pos = pos_of(&p);
            if self.head_full {
                // Offered after the head filled up: left out.
                if !p.text.trim().is_empty() {
                    self.head_refused_at.get_or_insert(pos.unwrap_or((seq, 0)));
                    break;
                }
                continue;
            }
            if let (Some(floor), Some(at)) = (self.tail_first, pos)
                && at >= floor
            {
                self.head_reached_floor = true;
                break;
            }
            if p.message.is_none() {
                p.text = normal_text(&p.text);
            }
            if p.text.is_empty() {
                continue;
            }
            let room = HEAD_USABLE.saturating_sub(self.trusted_bytes() + self.head_bytes);
            if let (Some(last), Some(m)) = (self.head.last_mut(), &p.message)
                && last.message.as_ref() == Some(m)
            {
                let cap = PIECE_MAX_BYTES.saturating_sub(last.text.len());
                let want = clip_at_word(&p.text, cap);
                if want.len() < p.text.len() {
                    self.clipped = true;
                }
                let more = clip_at_word(want, room);
                if more.len() < want.len() {
                    self.head_full = true;
                    self.head_refused_at.get_or_insert(pos.unwrap_or((seq, 0)));
                }
                last.text.push_str(more);
                self.head_bytes += more.len();
                self.head_last_seq = Some(seq);
                if self.head_full {
                    break;
                }
                continue;
            }
            if self.repeats(&p) {
                continue;
            }
            let want = clip_at_word(&p.text, PIECE_MAX_BYTES);
            if want.len() < p.text.len() {
                self.clipped = true;
            }
            if room < MIN_HEAD_CLIP {
                self.head_full = true;
                self.head_refused_at.get_or_insert(pos.unwrap_or((seq, 0)));
                break;
            }
            // The head keeps the start of a piece that does not fit whole.
            let keep = clip_at_word(want, room - 1).to_string();
            if keep.len() < want.len() {
                self.head_full = true;
                self.head_refused_at.get_or_insert(pos.unwrap_or((seq, 0)));
            }
            self.remember(&p);
            self.head_bytes += keep.len() + 1;
            self.head.push(Piece { text: keep, ..p });
            self.head_last_seq = Some(seq);
            if self.head_full {
                break;
            }
        }
        !self.head_open()
    }

    /// Adds the pieces of the next-newest chunk to the tail (in file order; chunks arrive newest first); returns whether it is full.
    /// A piece that does not fit is not taken: the tail stops there, [`BodyBuilder::tail_cut_seq`] names the chunk, and the head
    /// covers what lies before it.
    pub fn tail_chunk(&mut self, seq: u64, pieces: Vec<Piece>) -> bool {
        if self.tail_full {
            return true;
        }
        for mut p in pieces.into_iter().rev() {
            let pos = pos_of(&p);
            if p.message.is_none() {
                p.text = normal_text(&p.text);
            }
            if p.text.is_empty() {
                continue;
            }
            let room = TAIL_BYTES.saturating_sub(self.tail_bytes);
            if let (Some(first), Some(m)) = (self.tail_rev.last_mut(), &p.message)
                && first.message.as_ref() == Some(m)
            {
                // An older fragment of the message the tail already starts with: it goes in front.
                let cap = PIECE_MAX_BYTES.saturating_sub(first.text.len());
                let older = clip_tail_at_word(&p.text, cap);
                if older.len() < p.text.len() {
                    self.clipped = true;
                }
                if older.is_empty() {
                    continue;
                }
                if older.len() > room {
                    self.tail_full = true;
                    self.tail_cut = Some(seq);
                    break;
                }
                first.text.insert_str(0, older);
                // The message starts where its oldest fragment is.
                first.source = p.source.clone();
                self.tail_bytes += older.len();
                self.tail_first = pos;
                self.tail_first_seq = Some(seq);
                continue;
            }
            if self.repeats(&p) {
                continue;
            }
            let want = clip_tail_at_word(&p.text, PIECE_MAX_BYTES);
            if want.len() < p.text.len() {
                self.clipped = true;
            }
            if want.len() + 1 > room {
                self.tail_full = true;
                self.tail_cut = Some(seq);
                break;
            }
            self.remember(&p);
            self.tail_bytes += want.len() + 1;
            self.tail_rev.push(Piece {
                text: want.to_string(),
                ..p
            });
            self.tail_first = pos;
            self.tail_first_seq = Some(seq);
        }
        self.tail_full
    }

    /// The chunk the tail stopped inside: pieces of it before the tail's first piece are not in the tail, and the head should be fed
    /// that chunk too. `None` when the tail took every piece it was offered.
    pub fn tail_cut_seq(&self) -> Option<u64> {
        self.tail_cut
    }

    /// Whether more chunks could still add to the head, and to the tail.
    pub fn head_open(&self) -> bool {
        !self.head_full && !self.head_reached_floor
    }
    pub fn tail_open(&self) -> bool {
        !self.tail_full
    }

    /// The body, unredacted; see [`BodyBuilder::finish_with`].
    pub fn finish(self, all_read: bool) -> SearchBody {
        self.finish_with(all_read, &|s: &str| s.to_string())
    }

    /// The body. `all_read` says every chunk of the run was read without a fault; the body is [`Coverage::Full`] only then and
    /// only if nothing was cut to a limit and nothing lies between the head and the tail that neither holds.
    ///
    /// `redact` is applied to the text of each piece after the fragments of one message were joined (a secret split across two
    /// fragments is whole by then), and the body is fitted to [`BODY_MAX_BYTES`] afterwards, because redaction can make text longer:
    /// pieces next to the seam between head and tail are dropped, and the gap marker says so.
    pub fn finish_with(self, all_read: bool, redact: &dyn Fn(&str) -> String) -> SearchBody {
        let covered_by_tail = |at: Pos| self.tail_first.is_some_and(|first| at >= first);
        let head_lost = self.head_refused_at.is_some_and(|at| !covered_by_tail(at));
        let tail_lost = self.tail_cut.is_some() && !self.head_reached_floor;
        let seq_gap = !self.head_reached_floor
            && matches!((self.head_last_seq, self.tail_first_seq), (Some(h), Some(t)) if t > h + 1);
        let mut middle_lost = head_lost || tail_lost || seq_gap;
        let clipped = self.clipped;

        let mut trusted = self.trusted;
        let mut head = self.head;
        let mut tail: Vec<Piece> = self.tail_rev.into_iter().rev().collect();
        // A piece the head and the tail both hold (they were fed overlapping chunks) is kept in the head only.
        if let Some(last) = head.iter().rev().find_map(pos_of) {
            tail.retain(|p| pos_of(p).is_none_or(|at| at > last));
        }
        // A message that straddles the seam (the head ends with its early fragments, the tail starts with its later ones, nothing
        // lost between them) is joined here, so that the redaction sees it whole.
        let straddles = !middle_lost
            && matches!((head.last(), tail.first()), (Some(h), Some(t)) if h.message.is_some() && h.message == t.message);
        if straddles {
            let later = tail.remove(0);
            if let Some(earlier) = head.last_mut() {
                earlier.text.push_str(&later.text);
            }
        }
        // Message fragments were joined as written (a fragment may end inside a word); whitespace is collapsed now and the text
        // is redacted whole.
        for p in trusted
            .iter_mut()
            .chain(head.iter_mut())
            .chain(tail.iter_mut())
        {
            p.text = normal_text(&redact(&normal_text(&p.text)));
        }
        trusted.retain(|p| !p.text.is_empty());
        head.retain(|p| !p.text.is_empty());
        tail.retain(|p| !p.text.is_empty());

        let size = |v: &[Piece]| v.iter().map(|p| p.text.len() + 1).sum::<usize>();
        let marker_cost = GAP_MARKER.len() + 1;
        let mut trusted_b = size(&trusted);
        let mut head_b = size(&head);
        let mut tail_b = size(&tail);
        let (mut head_end, mut tail_start) = (head.len(), 0usize);
        let total =
            |t: usize, h: usize, l: usize, m: bool| t + h + l + if m { marker_cost } else { 0 };
        // Redaction can make text longer: drop the pieces next to the seam, the side that is over its share first.
        while total(trusted_b, head_b, tail_b, middle_lost) > BODY_MAX_BYTES {
            middle_lost = true;
            let head_left = head_end > 0;
            let tail_left = tail_start < tail.len();
            let drop_head = match (head_left, tail_left) {
                (true, true) => {
                    if trusted_b + head_b > HEAD_BYTES {
                        true
                    } else if tail_b > TAIL_BYTES {
                        false
                    } else {
                        trusted_b + head_b >= tail_b
                    }
                }
                (true, false) => true,
                (false, true) => false,
                (false, false) => break,
            };
            if drop_head {
                head_end -= 1;
                head_b -= head[head_end].text.len() + 1;
            } else {
                tail_b -= tail[tail_start].text.len() + 1;
                tail_start += 1;
            }
        }
        // The user's own words alone cannot exceed the limit, but the limit is held regardless.
        while total(trusted_b, head_b, tail_b, middle_lost) > BODY_MAX_BYTES {
            let Some(p) = trusted.pop() else { break };
            trusted_b -= p.text.len() + 1;
            middle_lost = true;
        }

        let mut pieces: Vec<Piece> = trusted;
        pieces.extend(head.into_iter().take(head_end));
        let tail_part: Vec<Piece> = tail.into_iter().skip(tail_start).collect();
        if middle_lost && (!pieces.is_empty() || !tail_part.is_empty()) {
            pieces.push(Piece {
                source: Source::Gap,
                text: GAP_MARKER.into(),
                message: None,
            });
        }
        pieces.extend(tail_part);
        let coverage = if all_read && !clipped && !middle_lost {
            Coverage::Full
        } else {
            Coverage::Partial
        };
        let mut through: Option<u64> = None;
        for p in &pieces {
            if let Source::Engine { seq, .. } = p.source {
                through = Some(through.map_or(seq, |t| t.max(seq)));
            }
        }
        let mut body = String::new();
        let mut segs = Vec::new();
        for p in &pieces {
            if segs.len() < SEGS_MAX {
                let (seq, rec) = match p.source {
                    Source::Engine { seq, rec } => (Some(seq), rec),
                    _ => (None, 0),
                };
                segs.push(Seg {
                    start: u32::try_from(body.len()).unwrap_or(u32::MAX),
                    seq,
                    rec,
                });
            }
            body.push_str(&p.text);
            body.push('\n');
        }
        SearchBody {
            body,
            segs,
            through,
            coverage,
        }
    }
}

// ---- matching --------------------------------------------------------------------------------------------------------

/// A character compared without case (the first character of its lowercase form, so byte positions can be mapped back).
pub fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Why a query was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("the query is empty")]
    Empty,
    #[error("the query is longer than {} characters", MAX_Q_CHARS)]
    TooLong,
    #[error("the query has more than {} words", MAX_TERMS)]
    TooMany,
    #[error("the query holds a control character")]
    Control,
}

/// The words of a query, each folded to lowercase. Every word must occur for a run to match.
pub fn parse_query(q: &str) -> Result<Vec<Vec<char>>, QueryError> {
    if q.chars().count() > MAX_Q_CHARS {
        return Err(QueryError::TooLong);
    }
    if q.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return Err(QueryError::Control);
    }
    let terms: Vec<Vec<char>> = q
        .split_whitespace()
        .map(|w| w.chars().map(fold).collect())
        .collect();
    if terms.is_empty() {
        return Err(QueryError::Empty);
    }
    if terms.len() > MAX_TERMS {
        return Err(QueryError::TooMany);
    }
    Ok(terms)
}

/// Where a match was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HitField {
    Chat,
    Prompt,
    Repository,
    Model,
}

/// One run's match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub field: HitField,
    pub snippet: String,
    /// The chunk and record where the matching piece starts (a chat match whose place is known).
    pub seq: Option<u64>,
    pub offset: Option<u32>,
}

fn folded(text: &str) -> Vec<(usize, char)> {
    text.char_indices().map(|(i, c)| (i, fold(c))).collect()
}

fn find(hay: &[(usize, char)], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .find(|&i| needle.iter().enumerate().all(|(j, c)| hay[i + j].1 == *c))
}

fn single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The words around char index `at` of `text`, about [`SNIPPET_CHARS`] of them, with `…` where it is cut.
fn snippet_of(text: &str, at: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(SNIPPET_CHARS / 3);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let start = end.saturating_sub(SNIPPET_CHARS).min(start);
    let mut s = String::new();
    if start > 0 {
        s.push_str(GAP_MARKER);
    }
    s.extend(&chars[start..end]);
    if end < chars.len() {
        s.push_str(GAP_MARKER);
    }
    single_line(&s)
}

/// Matches `item` against the folded `terms`: every term must occur in the body, the prompt, the repository or the model. The match
/// is reported in the chat body when the first term occurs there (with the place of its piece), else in the first field that has it.
pub fn find_match(item: &SearchItem, terms: &[Vec<char>]) -> Option<Match> {
    find_match_redacted(item, terms, &|s: &str| s.to_string())
}

/// The char index in `text` where the earliest of `terms` starts; 0 when none occurs (a match that exists only inside text the
/// redaction replaced).
fn locate(text: &str, terms: &[&Vec<char>]) -> usize {
    let hay = folded(text);
    terms
        .iter()
        .filter_map(|t| find(&hay, t))
        .min()
        .unwrap_or(0)
}

/// [`find_match`] with the snippet taken from `redact`ed text: the piece (or field) that holds the match is redacted whole before
/// the snippet is clipped, so a secret stored in a body can never be shown in part. The run matches on the stored text; when the
/// match lay inside text the redaction replaced, the snippet starts at the beginning of the piece.
pub fn find_match_redacted(
    item: &SearchItem,
    terms: &[Vec<char>],
    redact: &dyn Fn(&str) -> String,
) -> Option<Match> {
    if terms.is_empty() {
        return None;
    }
    let body = item.body.as_deref().unwrap_or("");
    let fields: [(HitField, &str); 4] = [
        (HitField::Chat, body),
        (HitField::Prompt, item.prompt_head.as_deref().unwrap_or("")),
        (
            HitField::Repository,
            item.repository.as_deref().unwrap_or(""),
        ),
        (HitField::Model, item.model.as_deref().unwrap_or("")),
    ];
    let hay: Vec<Vec<(usize, char)>> = fields.iter().map(|(_, t)| folded(t)).collect();
    // Every term must occur somewhere.
    for term in terms {
        if !hay.iter().any(|h| find(h, term).is_some()) {
            return None;
        }
    }
    // The earliest occurrence in the chat body of any term decides the place; failing that, the first term's field.
    let chat_hit = terms.iter().filter_map(|t| find(&hay[0], t)).min();
    let (field, char_at) = match chat_hit {
        Some(i) => (HitField::Chat, i),
        None => {
            let first = &terms[0];
            let (idx, at) = hay
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(k, h)| find(h, first).map(|i| (k, i)))?;
            (fields[idx].0, at)
        }
    };
    if field != HitField::Chat {
        let text = fields.iter().find(|f| f.0 == field)?.1;
        let shown = redact(text);
        // The original position only names the field; in the redacted text the first term is looked up again.
        let at = if shown == text {
            char_at
        } else {
            locate(&shown, &[&terms[0]])
        };
        return Some(Match {
            field,
            snippet: snippet_of(&shown, at),
            seq: None,
            offset: None,
        });
    }
    // Chat: clip the snippet to the piece that holds the match and report where the piece starts.
    let byte_at = hay[0].get(char_at).map_or(0, |(b, _)| *b);
    let segs = item.segs.as_deref().map(segs_from_str).unwrap_or_default();
    let idx = segs.iter().rposition(|s| s.start as usize <= byte_at);
    let (piece_start, piece_end) = match idx {
        Some(i) => {
            let end = segs.get(i + 1).map_or(body.len(), |n| n.start as usize);
            (segs[i].start as usize, end.min(body.len()))
        }
        None => (0, body.len()),
    };
    let piece = body
        .get(piece_start..piece_end)
        .unwrap_or(body)
        .trim_end_matches('\n');
    let shown = redact(piece);
    let at_in_piece = if shown == piece {
        body.get(piece_start..byte_at)
            .map_or(0, |s| s.chars().count())
    } else {
        let all: Vec<&Vec<char>> = terms.iter().collect();
        locate(&shown, &all)
    };
    let (seq, offset) = idx
        .and_then(|i| segs.get(i))
        .map_or((None, None), |s| (s.seq, s.seq.map(|_| s.rec)));
    Some(Match {
        field,
        snippet: snippet_of(&shown, at_in_piece),
        seq,
        offset,
    })
}

// ---- the wire --------------------------------------------------------------------------------------------------------

/// One run in a search response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub run_id: String,
    pub created_at: String,
    pub field: HitField,
    pub snippet: String,
    /// The chunk and record where the matching piece starts, for a chat match whose place is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Pass as `after` to the transcript route to open at the match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<String>,
    /// How much of the run's conversation is searchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
}

/// `GET /v1/search`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// One per matching run, newest run first.
    #[serde(default)]
    pub hits: Vec<SearchHit>,
    /// How many runs were examined.
    #[serde(default)]
    pub searched: u32,
    /// The search stopped at its deadline: older runs were not examined.
    #[serde(default)]
    pub partial: bool,
    /// Older runs remain beyond the limit; pass `next` as `after` to continue.
    #[serde(default)]
    pub more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Examined runs whose conversation is only partly searchable.
    #[serde(default)]
    pub partial_coverage: u32,
}

/// The query of `GET /v1/search`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub q: String,
    /// A date (`2026-09-01`) or a UTC time: runs created before it are not searched.
    pub since: Option<String>,
    pub limit: Option<u32>,
    pub after: Option<String>,
}

impl SearchQuery {
    /// The request path with its query string.
    pub fn path(&self) -> String {
        let mut parts = vec![format!(
            "q={}",
            crate::transcript::encode_component(&self.q)
        )];
        if let Some(s) = &self.since {
            parts.push(format!("since={}", crate::transcript::encode_component(s)));
        }
        if let Some(l) = self.limit {
            parts.push(format!("limit={l}"));
        }
        if let Some(a) = &self.after {
            parts.push(format!("after={}", crate::transcript::encode_component(a)));
        }
        format!("/v1/search?{}", parts.join("&"))
    }
}

/// Reads a response body.
pub fn parse_response(body: &[u8]) -> Result<SearchResponse, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}
