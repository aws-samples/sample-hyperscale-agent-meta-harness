//! The stored conversation as the chat view shows it.
//!
//! Pages of records from `GET /v1/runs/{id}/transcript` ([`crate::transcript::TranscriptPage`]) are collected in a [`StoredChat`]: records
//! by chunk number and record number, each identified by its writer's incarnation and number so a page that overlaps another adds nothing
//! twice, and the holes the route named, so a missing or unreadable chunk is a visible line and never silence. [`StoredChat::blocks`]
//! turns the records into the blocks of [`crate::chat`]: a message's fragments join, a tool call and its updates are one block, a diff in
//! a tool call is a diff block, and a kind this client has no name for is kept collapsed under its label.
//!
//! **Trust.** The chunks sit in a prefix the engine can write, so every record is engine origin whatever it says about itself. A block of
//! the stored conversation therefore never has a user or harness origin, wears the engine's role word, and carries no chrome: no card, no
//! key, no command. The only blocks with a harness look are gaps, and their words come from a fixed table with numbers read as numbers;
//! no text of a record reaches them. The keys on the run screen come from the event question state machine (`RunView.input`) and from
//! nothing in this module.
//!
//! **Tracks.** [`compose`] merges the three tracks for display without changing an origin (R8): the engine's blocks in record order, the
//! user's own items (the run's prompt and the messages it sent) and the harness's notices (events with harness authority) placed by
//! time. Times are display metadata: they never reorder the engine's own records. The progress feed supplies the prompt and the notices
//! only; its message tails, tool rows and final message are what the stored conversation replaces (R10), except that the newest tail is
//! shown as a labelled live line while the store has not caught up with it.

use crate::chat::{Block, BlockKind, ChatModel, GapCause, Origin, Role, Source};
use crate::tables::group_digits;
use crate::transcript::{
    CompletenessEntry, GapKind, TranscriptGap, TranscriptRecord, TranscriptState,
};
use harness_protocol::common::parse_utc_unix;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// How a page was read, for the cursors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// The first page of the view: from the end of the run, or from a search hit.
    First,
    /// A page of older chunks (`before`).
    Older,
    /// A page of newer chunks (`after`).
    Newer,
}

/// Records and gaps read so far.
#[derive(Clone, Debug, Default)]
pub struct StoredChat {
    /// By chunk number, record number and incarnation.
    records: BTreeMap<(u64, u64, String), TranscriptRecord>,
    seen: HashSet<(String, u64)>,
    gaps: Vec<TranscriptGap>,
    completeness: Vec<CompletenessEntry>,
    state: Option<TranscriptState>,
    older: Option<u64>,
    newer: Option<String>,
}

/// The words of a gap. Numbers only: nothing a record says is repeated.
fn range_words(from: u64, to: u64) -> String {
    if from == to {
        format!("chunk {from}")
    } else {
        format!("chunks {from} to {to}")
    }
}

impl StoredChat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a page. Records already held (same incarnation and number) are not added again; gaps already held are not added again; the
    /// state and the declared completeness are those of the newest page read.
    pub fn absorb(&mut self, page: &crate::transcript::TranscriptPage, how: Read) {
        for r in &page.records {
            if self.seen.insert((r.incarnation.clone(), r.n)) {
                self.records
                    .insert((r.seq, r.n, r.incarnation.clone()), r.clone());
            }
        }
        for g in &page.gaps {
            if !self.gaps.contains(g) {
                self.gaps.push(g.clone());
            }
        }
        self.completeness = page.completeness.clone();
        self.state = Some(page.state);
        match how {
            Read::First => {
                self.older = page.prev;
                self.newer = page.next.clone();
            }
            Read::Older => self.older = page.prev,
            Read::Newer => self.newer = page.next.clone(),
        }
    }

    /// The records of the conversation since its last `session` record that update what the engine can do (`current_mode_update`,
    /// `available_commands_update`), in record order: the changes to apply over the capability snapshot of the session that is running.
    pub fn capability_updates(&self) -> Vec<&TranscriptRecord> {
        let mut out: Vec<&TranscriptRecord> = Vec::new();
        for r in self.records.values() {
            match r.kind.as_str() {
                "session" => out.clear(),
                "current_mode_update" | "available_commands_update" => out.push(r),
                _ => {}
            }
        }
        out
    }

    /// The state the newest page reported.
    pub fn state(&self) -> Option<TranscriptState> {
        self.state
    }

    /// Records held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.gaps.is_empty()
    }

    /// The `before` that reads the chunks older than the oldest page; `None` when there are none (or none is known).
    pub fn older(&self) -> Option<u64> {
        self.older
    }

    /// The `after` that reads on from the newest page; `None` when the run ended and everything was read.
    pub fn newer(&self) -> Option<String> {
        self.newer.clone()
    }

    /// The blocks of the engine track in order, with the holes in place.
    pub fn blocks(&self) -> Vec<Block> {
        self.timed().into_iter().map(|(_, b)| b).collect()
    }

    /// The blocks with the time (Unix seconds) of the record each began at, when it has one.
    pub fn timed(&self) -> Vec<(Option<i64>, Block)> {
        // The holes: what the pages named and the ranged losses the worker declared, in chunk order.
        let mut holes: Vec<Hole> = self
            .gaps
            .iter()
            .map(|g| Hole {
                from: g.from_seq,
                to: g.to_seq,
                cause: g.kind.cause(),
                order: format!("{:?}", g.code),
            })
            .collect();
        for e in &self.completeness {
            if let (Some(from), Some(to)) = (e.from_seq, e.to_seq)
                && !matches!(e.kind, GapKind::Excluded | GapKind::TailUnknown)
            {
                holes.push(Hole {
                    from,
                    to,
                    cause: e.kind.cause(),
                    order: format!("declared {:?}", e.kind),
                });
            }
        }
        holes.sort_by(|a, b| (a.from, a.to, &a.order).cmp(&(b.from, b.to, &b.order)));
        let mut pj = Projector::default();
        let mut next_hole = 0;
        for ((seq, _, _), rec) in &self.records {
            while next_hole < holes.len() && holes[next_hole].from <= *seq {
                pj.hole(&holes[next_hole]);
                next_hole += 1;
            }
            pj.record(rec);
        }
        while next_hole < holes.len() {
            pj.hole(&holes[next_hole]);
            next_hole += 1;
        }
        pj.declared(&self.completeness);
        pj.finish()
    }
}

/// A range of chunks that is not shown.
#[derive(Debug)]
struct Hole {
    from: u64,
    to: u64,
    cause: GapCause,
    /// Breaks ties between holes of the same range, so the order does not depend on the order the pages arrived in.
    order: String,
}

// ---- projection ---------------------------------------------------------------------------------------------------------------

/// Text of `v` as a string when it is one.
fn text_of(v: &Value) -> Option<&str> {
    v.as_str()
}

/// A compact JSON text of `v`, cut at `max` characters.
fn compact(v: &Value, max: usize) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.chars().count() <= max {
        s
    } else {
        let mut cut: String = s.chars().take(max).collect();
        cut.push('\u{2026}');
        cut
    }
}

/// The text of an ACP content block: its `text`, or a word for the kind of thing it is.
fn content_text(c: &Value) -> String {
    match c.get("type").and_then(text_of) {
        Some("text") => c
            .get("text")
            .and_then(text_of)
            .unwrap_or_default()
            .to_string(),
        Some(other) => format!("[{other} content]"),
        None => String::new(),
    }
}

/// What a tool call has said so far.
#[derive(Clone, Debug, Default)]
struct Tool {
    index: usize,
    id: String,
    title: String,
    kind: Option<String>,
    status: Option<String>,
    input: Option<String>,
    outputs: Vec<String>,
    diffs: HashSet<u64>,
}

impl Tool {
    fn body(&self) -> String {
        let mut parts = Vec::new();
        if let Some(i) = &self.input {
            parts.push(format!("Input\n{i}"));
        }
        if !self.outputs.is_empty() {
            parts.push(format!("Output\n{}", self.outputs.join("\n")));
        }
        parts.join("\n\n")
    }
}

/// An input object as a few readable lines: `$ command` for a command, `key: value` otherwise.
fn input_text(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            if let Some(cmd) = map.get("command").and_then(text_of) {
                let mut s = format!("$ {cmd}");
                for (k, val) in map.iter().filter(|(k, _)| k.as_str() != "command").take(6) {
                    s.push_str(&format!("\n{k}: {}", scalar_or_json(val)));
                }
                return s;
            }
            map.iter()
                .take(8)
                .map(|(k, val)| format!("{k}: {}", scalar_or_json(val)))
                .collect::<Vec<_>>()
                .join("\n")
        }
        Value::String(s) => s.clone(),
        other => compact(other, 1000),
    }
}

fn scalar_or_json(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => compact(other, 600),
    }
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0x1f)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// A small line diff of `old` against `new` for the diff block: `---` and `+++` lines, then each line marked ` `, `-` or `+`; runs of
/// unchanged lines far from a change are replaced by one `@@` line. Text too large for the table is shown as a count.
pub fn line_diff(path: &str, old: &str, new: &str) -> String {
    const MAX_CELLS: usize = 1_000_000;
    const CONTEXT: usize = 3;
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = format!("--- {path}\n+++ {path}\n");
    if a.len().saturating_mul(b.len()) > MAX_CELLS {
        out.push_str(&format!(
            "@@ too large to compare: {} lines before, {} after @@",
            a.len(),
            b.len()
        ));
        return out;
    }
    // Longest common subsequence table, filled from the end.
    let width = b.len() + 1;
    let mut table = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i * width + j] = if a[i] == b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut lines: Vec<(char, &str)> = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            lines.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            lines.push(('-', a[i]));
            i += 1;
        } else {
            lines.push(('+', b[j]));
            j += 1;
        }
    }
    lines.extend(a[i..].iter().map(|l| ('-', *l)));
    lines.extend(b[j..].iter().map(|l| ('+', *l)));
    let keep: Vec<bool> = (0..lines.len())
        .map(|k| {
            let lo = k.saturating_sub(CONTEXT);
            let hi = (k + CONTEXT).min(lines.len().saturating_sub(1));
            lines[lo..=hi].iter().any(|(m, _)| *m != ' ')
        })
        .collect();
    let mut skipped = 0usize;
    for (k, (mark, line)) in lines.iter().enumerate() {
        if keep[k] {
            if skipped > 0 {
                out.push_str(&format!("@@ {skipped} unchanged lines @@\n"));
                skipped = 0;
            }
            out.push(*mark);
            out.push_str(line);
            out.push('\n');
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push_str(&format!("@@ {skipped} unchanged lines @@\n"));
    }
    out.trim_end_matches('\n').to_string()
}

#[derive(Debug, Default)]
struct Projector {
    out: Vec<(Option<i64>, Block)>,
    /// The message in progress: its engine message id, the incarnation, the index in `out` and the text so far.
    message: Option<(String, String, usize, String)>,
    tools: HashMap<String, Tool>,
    turns: u32,
    gap_serial: u32,
}

impl Projector {
    fn finish(self) -> Vec<(Option<i64>, Block)> {
        self.out
    }

    fn push(&mut self, t: Option<i64>, b: Block) {
        self.message = None;
        self.out.push((t, b));
    }

    fn hole(&mut self, h: &Hole) {
        self.gap_serial += 1;
        let id = format!("gap:{}:{}:{}", h.from, h.to, self.gap_serial);
        let b = Block::gap(&id, h.cause, Some(&range_words(h.from, h.to)));
        self.push(None, b);
    }

    /// The completeness list the worker declared: a loss with a count and no range is a line at the end;
    /// what was left out on purpose (thoughts, binary output) says nothing; an abnormal end is the last line.
    fn declared(&mut self, entries: &[CompletenessEntry]) {
        let mut tail_unknown = false;
        let mut counted: Vec<&CompletenessEntry> = Vec::new();
        for e in entries {
            match e.kind {
                GapKind::Excluded => {}
                GapKind::TailUnknown => tail_unknown = true,
                // A ranged loss is placed among the chunks (`StoredChat::timed`).
                _ if e.from_seq.is_some() && e.to_seq.is_some() => {}
                _ => counted.push(e),
            }
        }
        for e in counted {
            self.gap_serial += 1;
            let detail = e.count.map(|n| format!("{} records", group_digits(n)));
            let id = format!("declared:{}", self.gap_serial);
            let b = Block::gap(&id, e.kind.cause(), detail.as_deref());
            self.push(None, b);
        }
        if tail_unknown {
            let b = Block::gap("declared:tail-unknown", GapCause::TailUnknown, None);
            self.push(None, b);
        }
    }

    fn record(&mut self, r: &TranscriptRecord) {
        let t = r.t.as_deref().and_then(parse_utc_unix);
        let id = Block::record_id(&r.incarnation, r.n);
        let d = &r.data;
        match r.kind.as_str() {
            "agent_message_chunk" => self.agent_message(r, t, &id),
            "user_message_chunk" => {
                let text = d.get("content").map(content_text).unwrap_or_default();
                let b = Block::message(&id, Role::Engine, Origin::Engine, &text);
                self.push(t, with_author(b, d));
            }
            "agent_thought_chunk" => {
                let text = d.get("content").map(content_text).unwrap_or_default();
                self.push(t, Block::thought(&id, &text));
            }
            "tool_call" | "tool_call_update" => self.tool(r, t, &id),
            "plan" => {
                let entries: Vec<(String, String)> = d
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|e| {
                                (
                                    e.get("status")
                                        .and_then(text_of)
                                        .unwrap_or_default()
                                        .to_string(),
                                    e.get("content")
                                        .and_then(text_of)
                                        .unwrap_or_default()
                                        .to_string(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let refs: Vec<(&str, &str)> = entries
                    .iter()
                    .map(|(s, c)| (s.as_str(), c.as_str()))
                    .collect();
                self.push(t, Block::plan(&id, &refs));
            }
            "usage_update" => {
                let n = |k: &str| d.get(k).and_then(Value::as_u64);
                let mut line = match (n("used"), n("size")) {
                    (Some(u), Some(s)) => format!(
                        "context: {} of {} tokens used",
                        group_digits(u),
                        group_digits(s)
                    ),
                    (Some(u), None) => format!("context: {} tokens used", group_digits(u)),
                    _ => "usage update".to_string(),
                };
                if let Some(amount) = d.pointer("/cost/amount").and_then(Value::as_f64) {
                    let cur = d
                        .pointer("/cost/currency")
                        .and_then(text_of)
                        .filter(|c| c.chars().all(|ch| ch.is_ascii_alphabetic()) && c.len() <= 8)
                        .unwrap_or("");
                    line.push_str(&format!(" \u{b7} cost {amount:.4} {cur}"));
                }
                self.push(t, Block::usage(&id, line.trim_end()));
            }
            "prompt" => {
                let text = d
                    .get("prompt")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(content_text).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                self.push(t, Block::unknown(&id, "prompt sent to the engine", &text));
            }
            "prompt_result" => {
                self.turns += 1;
                let k = self.turns;
                let line = if let Some(stop) = d.pointer("/result/stopReason").and_then(text_of) {
                    let mut l = format!("turn {k} ended: {stop}");
                    if let Some(total) = d
                        .pointer("/result/usage/totalTokens")
                        .and_then(Value::as_u64)
                    {
                        l.push_str(&format!(" \u{b7} {} tokens", group_digits(total)));
                    }
                    l
                } else if d.get("error").is_some() {
                    let code = d.pointer("/error/code").and_then(Value::as_i64);
                    match code {
                        Some(c) => format!("turn {k} failed: the engine answered with error {c}"),
                        None => format!("turn {k} failed: the engine answered with an error"),
                    }
                } else {
                    format!("turn {k} ended")
                };
                self.push(t, Block::usage(&id, &line));
            }
            "cancel" => self.push(t, Block::usage(&id, "cancel sent to the engine")),
            "permission_request" => {
                let title = d
                    .pointer("/params/toolCall/title")
                    .and_then(text_of)
                    .unwrap_or("a tool");
                self.push(
                    t,
                    Block::unknown(
                        &id,
                        &format!("permission requested: {title}"),
                        &compact(d, 1200),
                    ),
                );
            }
            "permission_response" => self.push(
                t,
                Block::unknown(&id, "permission answered", &compact(d, 600)),
            ),
            "session" => {
                let how = d.get("method").and_then(text_of).unwrap_or("session/new");
                let word = if how.ends_with("load") {
                    "session resumed"
                } else {
                    "session started"
                };
                self.push(t, Block::usage(&id, word));
            }
            "end" => self.push(t, Block::usage(&id, "the conversation was closed cleanly")),
            "current_mode_update" => {
                let mode = d
                    .get("currentModeId")
                    .and_then(text_of)
                    .unwrap_or("changed");
                self.push(t, Block::usage(&id, &format!("mode: {mode}")));
            }
            "gap" => self.gap_record(t, &id, d),
            "omitted" => {
                let bytes = d.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                let what = d.get("kind").and_then(text_of).unwrap_or("a");
                let detail = format!(
                    "a {} record of {} bytes was too large to show",
                    crate::transcript::clean_kind(what),
                    group_digits(bytes)
                );
                self.push(t, Block::gap(&id, GapCause::Truncated, Some(&detail)));
            }
            "opaque" => {
                let name = d
                    .get("method")
                    .or_else(|| d.get("variant"))
                    .and_then(text_of)
                    .unwrap_or("message");
                let bytes = d.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                let payload = d
                    .get("payload")
                    .map(|p| compact(p, 1200))
                    .unwrap_or_default();
                self.push(
                    t,
                    Block::unknown(
                        &id,
                        &format!("opaque: {name} ({} bytes)", group_digits(bytes)),
                        &payload,
                    ),
                );
            }
            other => self.push(t, Block::unknown(&id, other, &compact(d, 1200))),
        }
    }

    fn gap_record(&mut self, t: Option<i64>, id: &str, d: &Value) {
        let kind = d
            .get("kind")
            .and_then(text_of)
            .and_then(|k| serde_json::from_value::<GapKind>(Value::String(k.to_string())).ok());
        let Some(kind) = kind else {
            self.push(
                t,
                Block::unknown(
                    id,
                    "gap record of a kind this client does not know",
                    &compact(d, 400),
                ),
            );
            return;
        };
        let mut parts = Vec::new();
        if let Some(n) = d.get("count").and_then(Value::as_u64) {
            parts.push(format!("{} records", group_digits(n)));
        }
        if let Some(a) = d.get("afterSeq").and_then(Value::as_u64) {
            parts.push(format!("after chunk {a}"));
        }
        let detail = (!parts.is_empty()).then(|| parts.join(", "));
        self.push(t, Block::gap(id, kind.cause(), detail.as_deref()));
    }

    fn agent_message(&mut self, r: &TranscriptRecord, t: Option<i64>, id: &str) {
        let d = &r.data;
        let content = d.get("content");
        let is_text = content
            .and_then(|c| c.get("type"))
            .and_then(text_of)
            .is_some_and(|k| k == "text");
        if !is_text {
            let label = content
                .and_then(|c| c.get("type"))
                .and_then(text_of)
                .map_or_else(
                    || "message".to_string(),
                    |k| format!("message with {k} content"),
                );
            self.push(t, Block::unknown(id, &label, &compact(d, 600)));
            return;
        }
        let text = content.map(content_text).unwrap_or_default();
        let mid = d
            .get("messageId")
            .and_then(text_of)
            .unwrap_or_default()
            .to_string();
        if let Some((open_mid, inc, index, acc)) = &mut self.message
            && !mid.is_empty()
            && *open_mid == mid
            && *inc == r.incarnation
        {
            acc.push_str(&text);
            let first_id = self.out[*index].1.id.clone();
            let b = with_author(
                Block::message(&first_id, Role::Engine, Origin::Engine, acc),
                d,
            );
            self.out[*index].1 = b;
            return;
        }
        let b = with_author(Block::message(id, Role::Engine, Origin::Engine, &text), d);
        self.out.push((t, b));
        self.message = Some((mid, r.incarnation.clone(), self.out.len() - 1, text));
    }

    fn tool(&mut self, r: &TranscriptRecord, t: Option<i64>, id: &str) {
        let d = &r.data;
        self.message = None;
        let call = d
            .get("toolCallId")
            .and_then(text_of)
            .unwrap_or_default()
            .to_string();
        // A call with no id is its own block and never merges.
        let key = if call.is_empty() {
            format!("anon:{id}")
        } else {
            call
        };
        let mut tool = self.tools.remove(&key).unwrap_or_else(|| Tool {
            index: usize::MAX,
            id: id.to_string(),
            ..Tool::default()
        });
        if let Some(title) = d
            .get("title")
            .and_then(text_of)
            .filter(|s| !s.trim().is_empty())
        {
            tool.title = title.to_string();
        }
        if let Some(k) = d.get("kind").and_then(text_of) {
            tool.kind = Some(k.to_string());
        }
        if let Some(s) = d.get("status").and_then(text_of) {
            tool.status = Some(s.to_string());
        }
        if let Some(i) = d.get("rawInput").filter(|v| !v.is_null()) {
            tool.input = Some(input_text(i));
        }
        let mut diffs: Vec<(String, String, String)> = Vec::new();
        if let Some(items) = d.get("content").and_then(Value::as_array) {
            for item in items {
                match item.get("type").and_then(text_of) {
                    Some("diff") => {
                        let path = item.get("path").and_then(text_of).unwrap_or_default();
                        let old = item.get("oldText").and_then(text_of).unwrap_or_default();
                        let new = item.get("newText").and_then(text_of).unwrap_or_default();
                        diffs.push((path.to_string(), old.to_string(), new.to_string()));
                    }
                    Some("content") => {
                        let text = item.get("content").map(content_text).unwrap_or_default();
                        if !text.is_empty() && !tool.outputs.contains(&text) {
                            tool.outputs.push(text);
                        }
                    }
                    Some("terminal") => {
                        let text = "[terminal output is not stored]".to_string();
                        if !tool.outputs.contains(&text) {
                            tool.outputs.push(text);
                        }
                    }
                    _ => {}
                }
            }
        }
        if tool.outputs.is_empty()
            && let Some(raw) = d.get("rawOutput").filter(|v| !v.is_null())
        {
            tool.outputs.push(scalar_or_json(raw));
        }
        let block = Block::tool_call(
            &tool.id,
            &tool.title,
            tool.kind.as_deref(),
            tool.status.as_deref(),
            &tool.body(),
        );
        if tool.index == usize::MAX {
            self.out.push((t, block));
            tool.index = self.out.len() - 1;
            self.message = None;
        } else {
            self.out[tool.index].1 = block;
        }
        for (path, old, new) in diffs {
            let h = hash_of(&[&path, &old, &new]);
            if tool.diffs.insert(h) {
                let did = format!("{}:d{}", tool.id, tool.diffs.len());
                let text = line_diff(&path, &old, &new);
                self.out.push((t, Block::diff(&did, &path, &text)));
                self.message = None;
            }
        }
        self.tools.insert(key, tool);
    }
}

/// The block with the advisory `author` its record named, when it named one as a string.
fn with_author(b: Block, d: &Value) -> Block {
    match d.get("author").and_then(text_of) {
        Some(a) => b.with_author(a),
        None => b,
    }
}

// ---- the three tracks ---------------------------------------------------------------------------------------------------------

/// A block of the user's own track (a message of the user, placed by the time the server accepted it).
#[derive(Clone, Debug, PartialEq)]
pub struct UserItem {
    /// RFC 3339 UTC.
    pub at: String,
    pub block: Block,
}

/// What [`compose`] reads besides the stored conversation.
#[derive(Debug)]
pub struct Context<'a> {
    /// The progress feed of the run (`ChatModel::from_progress_feed` without its `status` and `delivery` blocks): the source of the prompt
    /// and of the harness's notices, and of the live line.
    pub progress: &'a ChatModel,
    pub user: &'a [UserItem],
    /// The run has not ended: the newest message tail of the progress feed may be shown as a live line.
    pub live: bool,
}

/// Whether the view should be built from the stored conversation: the store supports it and holds something to show. A store that is
/// unsupported, purged, empty, or not read yet leaves the progress feed (and its label) in place.
pub fn uses_stored(stored: &StoredChat) -> bool {
    matches!(
        stored.state(),
        Some(TranscriptState::Live | TranscriptState::Incomplete | TranscriptState::Complete)
    ) && !stored.is_empty()
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The time (Unix seconds) in the first ten characters of a ULID.
pub fn ulid_secs(id: &str) -> Option<i64> {
    let head = id.get(..10)?;
    let mut ms: u64 = 0;
    for b in head.bytes() {
        let v = CROCKFORD
            .iter()
            .position(|c| *c == b.to_ascii_uppercase())?;
        ms = ms.checked_mul(32)?.checked_add(v as u64)?;
    }
    i64::try_from(ms / 1000).ok()
}

/// The ULID inside a progress-feed block id (`n:<ulid>`, `u:<ulid>`, `n:<ulid>:blocked`).
fn block_secs(id: &str) -> Option<i64> {
    ulid_secs(id.split_once(':')?.1)
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The view of a run that has a stored conversation: the prompt, the engine's blocks with the user's items and the harness's notices
/// between them, and a live line. A store that [`uses_stored`] says no to returns the progress feed unchanged.
pub fn compose(stored: &StoredChat, cx: &Context<'_>) -> ChatModel {
    if !uses_stored(stored) {
        return cx.progress.clone();
    }
    let progress = cx.progress;
    let mut extras: Vec<(Option<i64>, Block)> = Vec::new();
    for item in cx.user {
        extras.push((parse_utc_unix(&item.at), item.block.clone()));
    }
    for b in &progress.blocks {
        let notice = b.origin == Origin::Harness
            && (b.id.starts_with("n:") || b.id.starts_with("u:") || b.id == "goal:rounds");
        if notice {
            extras.push((block_secs(&b.id), b.clone()));
        }
    }
    // Stable by time; an item with no time goes last.
    extras.sort_by_key(|(t, _)| t.unwrap_or(i64::MAX));

    let mut blocks: Vec<Block> = Vec::new();
    if let Some(p) = progress.blocks.iter().find(|b| b.id == "prompt") {
        blocks.push(p.clone());
    }
    let engine = stored.timed();
    let mut extras = extras.into_iter().peekable();
    for (t, b) in &engine {
        if let Some(et) = t {
            while let Some((Some(xt), _)) = extras.peek() {
                if xt < et {
                    if let Some((_, x)) = extras.next() {
                        blocks.push(x);
                    }
                } else {
                    break;
                }
            }
        }
        blocks.push(b.clone());
    }
    blocks.extend(extras.map(|(_, x)| x));

    if cx.live
        && let Some(tail) = progress
            .blocks
            .iter()
            .rev()
            .find(|b| b.id.starts_with("m:") && b.kind == BlockKind::Message)
    {
        let want = squeeze(&tail.body);
        let covered = want.is_empty()
            || engine
                .iter()
                .rev()
                .filter(|(_, b)| b.kind == BlockKind::Message)
                .take(3)
                .any(|(_, b)| squeeze(&b.body).contains(&want));
        if !covered {
            let mut live = Block::unknown("live", "live \u{b7} not stored yet", &tail.body);
            live.folded = false;
            blocks.push(live);
        }
    }
    ChatModel::from_blocks(Source::FullHistory, &progress.engine, blocks)
}
