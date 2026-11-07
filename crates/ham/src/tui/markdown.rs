//! Markdown, code and diff rendering of transcript blocks as styled rows. The module draws nothing and reads no event: it
//! turns the text of a block into rows (a ratatui `Line` and a [`Join`] tag that says how the row continues the one before it, for
//! `select::TextLines`) that fit a width in display cells. No crate was added: the parser is small and written for what models write.
//!
//! * **Safe first.** The text goes through `display_text` before anything else, so no escape sequence, control character or bidirectional
//!   mark reaches a span (markers `<U+XXXX>` are shown instead). Links are text: `[docs](https://x)` is `docs (https://x)`, the address in
//!   dim; nothing is ever a hyperlink. Raw HTML and entities are shown as written.
//! * **Blocks.** Headings (`#` to `######`), paragraphs,
//!   fenced code (the closing fence is the same character and at least as long, so a longer fence holds shorter ones), indented code
//!   after a blank line, quotes and lists to a nesting depth of 8 (lazy continuation lines, task items as written, hanging indents),
//!   GFM tables (alignment, `\|`, `<br>`, wrapping inside cells; too narrow a screen shows the source lines), and rules. Setext
//!   headings are not supported: `---` is always a rule.
//! * **Inline.** Code spans, `*`/`_` emphasis and strong, `~~strike~~`, links, images, autolinks and backslash escapes, with the CommonMark
//!   delimiter rules (an unmatched marker stays visible) in a single pass, so the work stays linear.
//! * **Diffs.** [`render_diff`] colours a unified diff by line kind, follows the hunk counts so a removed line that looks like a file header is
//!   still a removal, and reverses the words that changed between a removed line and the added line paired with it. Text that is not a
//!   unified diff is shown raw. [`unified_diff`] makes a diff of two texts.
//! * **Bounds.** Every row fits the width; at most [`MAX_ROWS`] rows are returned with a note for the rest; [`render_counted`] counts the
//!   steps so a test holds the cost to a few steps a character.

use super::select::{Join, display_units};
use ham_core::chat::{Block, BlockKind, Role};
use ham_core::sanitize::display_text;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;

/// The most rows [`render`] returns; the rest is replaced by one note row.
pub const MAX_ROWS: usize = 20_000;
/// Quotes and lists nest to this depth; deeper marks are text.
const MAX_DEPTH: usize = 8;
const MAX_TABLE_COLS: usize = 40;
const TAB: usize = 4;
const RULE_MAX: usize = 40;
/// A pair of diff lines with more tokens than this gets colour and no word emphasis.
const DIFF_TOKENS_MAX: usize = 256;
/// The table cells (tokens times tokens) all word emphasis in one diff may fill.
const DIFF_BUDGET: usize = 2_000_000;

/// One row of output.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub line: Line<'static>,
    /// How this row continues the one before it (see [`Join`]).
    pub join: Join,
}

// ---- units, spans, rows -------------------------------------------------------------------------------------------------------------

/// One display unit (a character with its marks, or an emoji sequence) with its style and its cells.
#[derive(Clone, Debug)]
struct Unit {
    text: String,
    style: Style,
    w: usize,
    /// A space the wrapper may break at (a space inside a code span is not).
    space: bool,
}

fn units_of(s: &str, style: Style, breakable: bool) -> Vec<Unit> {
    display_units(s)
        .into_iter()
        .map(|(a, b, w)| {
            let t = &s[a..b];
            Unit {
                text: t.to_string(),
                style,
                w,
                space: breakable && t == " ",
            }
        })
        .collect()
}

fn ellipsis(style: Style) -> Unit {
    Unit {
        text: "\u{2026}".to_string(),
        style,
        w: 1,
        space: false,
    }
}

fn spans_of(units: &[Unit]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = Style::default();
    for u in units {
        if !cur.is_empty() && u.style != style {
            out.push(Span::styled(std::mem::take(&mut cur), style));
        }
        style = u.style;
        cur.push_str(&u.text);
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, style));
    }
    out
}

fn row_of(units: &[Unit], join: Join) -> Row {
    Row {
        line: Line::from(spans_of(units)),
        join,
    }
}

fn blank() -> Row {
    Row {
        line: Line::default(),
        join: Join::Hard,
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn code_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn rows_width(units: &[Unit]) -> usize {
    units.iter().map(|u| u.w).sum()
}

fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut col = 0usize;
    for c in line.chars() {
        if c == '\t' {
            let n = TAB - (col % TAB);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

// ---- the wrapper ----------------------------------------------------------------------------------------------------------------------

enum Tok {
    U(Unit),
    Break,
}

/// Fills rows greedily: words break at spaces, a word longer than the width starts a row and is cut, a line break ends a row.
struct Wrapper<'a> {
    width: usize,
    rows: Vec<(Vec<Unit>, Join)>,
    cur: Vec<Unit>,
    cur_w: usize,
    join: Join,
    sep: Option<Unit>,
    word: Vec<Unit>,
    word_w: usize,
    work: &'a mut u64,
}

impl<'a> Wrapper<'a> {
    fn new(width: usize, work: &'a mut u64) -> Self {
        Self {
            width,
            rows: Vec::new(),
            cur: Vec::new(),
            cur_w: 0,
            join: Join::Hard,
            sep: None,
            word: Vec::new(),
            word_w: 0,
            work,
        }
    }

    fn push_row(&mut self) {
        let row = std::mem::take(&mut self.cur);
        self.rows.push((row, self.join));
        self.cur_w = 0;
    }

    fn place(&mut self, word: Vec<Unit>, ww: usize) {
        if ww <= self.width {
            self.cur_w += ww;
            self.cur.extend(word);
            return;
        }
        for u in word {
            let u = if u.w > self.width {
                ellipsis(u.style)
            } else {
                u
            };
            if self.cur_w + u.w > self.width {
                self.push_row();
                self.join = Join::Cut;
            }
            self.cur_w += u.w;
            self.cur.push(u);
        }
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let ww = std::mem::take(&mut self.word_w);
        let sep = self.sep.take();
        if self.cur.is_empty() {
            self.place(word, ww);
        } else if self.cur_w + 1 + ww <= self.width {
            let mut s = sep.unwrap_or_else(|| units_of(" ", Style::default(), true).remove(0));
            s.w = 1;
            self.cur_w += 1;
            self.cur.push(s);
            self.place(word, ww);
        } else {
            self.push_row();
            self.join = Join::Space;
            self.place(word, ww);
        }
    }

    fn feed(&mut self, t: Tok) {
        *self.work += 1;
        match t {
            Tok::U(u) if u.space => {
                self.end_word();
                if self.sep.is_none() {
                    self.sep = Some(u);
                }
            }
            Tok::U(u) => {
                self.word_w += u.w;
                self.word.push(u);
            }
            Tok::Break => {
                self.end_word();
                self.sep = None;
                self.push_row();
                self.join = Join::Hard;
            }
        }
    }

    fn finish(mut self) -> Vec<(Vec<Unit>, Join)> {
        self.end_word();
        if !self.cur.is_empty() {
            self.push_row();
        }
        self.rows
    }
}

fn wrap(toks: Vec<Tok>, width: usize, work: &mut u64) -> Vec<(Vec<Unit>, Join)> {
    let mut w = Wrapper::new(width, work);
    for t in toks {
        w.feed(t);
    }
    w.finish()
}

fn wrapped_rows(toks: Vec<Tok>, width: usize, work: &mut u64) -> Vec<Row> {
    wrap(toks, width, work)
        .into_iter()
        .map(|(u, j)| row_of(&u, j))
        .collect()
}

/// Cuts `units` into rows of at most `width` cells (code, diff lines): no space is dropped.
fn cut_rows(units: Vec<Unit>, width: usize, work: &mut u64) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut cur: Vec<Unit> = Vec::new();
    let mut cur_w = 0usize;
    let mut join = Join::Hard;
    for u in units {
        *work += 1;
        let u = if u.w > width { ellipsis(u.style) } else { u };
        if cur_w + u.w > width {
            rows.push(row_of(&cur, join));
            cur.clear();
            cur_w = 0;
            join = Join::Cut;
        }
        cur_w += u.w;
        cur.push(u);
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(row_of(&cur, join));
    }
    rows
}

/// Text as it was written, wrapped at spaces without collapsing them: user and tool text.
fn plain_rows(text: &str, width: usize, style: Style, work: &mut u64) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let units = units_of(&expand_tabs(line), style, false);
        *work += units.len() as u64 + 1;
        let mut units = units;
        while units.last().is_some_and(|u| u.text == " ") {
            units.pop();
        }
        if units.is_empty() {
            rows.push(blank());
            continue;
        }
        let mut start = 0usize;
        let mut join = Join::Hard;
        while start < units.len() {
            let mut end = start;
            let mut w = 0usize;
            while end < units.len() {
                let uw = units[end].w.min(width);
                if w + uw > width {
                    break;
                }
                w += uw;
                end += 1;
            }
            if end == units.len() {
                rows.push(row_of(&fit(&units[start..end], width), join));
                break;
            }
            if let Some(sp) = (start + 1..end).rev().find(|k| units[*k].text == " ") {
                let mut part = units[start..sp].to_vec();
                while part.last().is_some_and(|u| u.text == " ") {
                    part.pop();
                }
                rows.push(row_of(&fit(&part, width), join));
                start = sp + 1;
                join = Join::Space;
            } else {
                if end == start {
                    end = start + 1;
                }
                rows.push(row_of(&fit(&units[start..end], width), join));
                start = end;
                join = Join::Cut;
            }
        }
    }
    rows
}

/// `units` with any unit wider than `width` replaced by an ellipsis.
fn fit(units: &[Unit], width: usize) -> Vec<Unit> {
    units
        .iter()
        .map(|u| {
            if u.w > width {
                ellipsis(u.style)
            } else {
                u.clone()
            }
        })
        .collect()
}

// ---- inline -----------------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
enum Node {
    Text(String),
    Code(String),
    Break,
    Delim(usize),
    Tail(String),
}

#[derive(Debug)]
struct Delim {
    ch: char,
    orig: usize,
    count: usize,
    can_open: bool,
    can_close: bool,
    node: usize,
    live: bool,
}

/// Difference arrays of the styles ranges of nodes get; summed once at the end.
#[derive(Default)]
struct Marks {
    em: Vec<i32>,
    strong: Vec<i32>,
    strike: Vec<i32>,
    link: Vec<i32>,
}

#[derive(Clone, Copy)]
enum Mark {
    Em,
    Strong,
    Strike,
    Link,
}

impl Marks {
    fn new() -> Self {
        Self {
            em: vec![0],
            strong: vec![0],
            strike: vec![0],
            link: vec![0],
        }
    }

    fn grow(&mut self) {
        self.em.push(0);
        self.strong.push(0);
        self.strike.push(0);
        self.link.push(0);
    }

    fn truncate(&mut self, nodes: usize) {
        for v in [
            &mut self.em,
            &mut self.strong,
            &mut self.strike,
            &mut self.link,
        ] {
            v.truncate(nodes + 1);
        }
    }

    /// Marks nodes `a..b`.
    fn add(&mut self, kind: Mark, a: usize, b: usize) {
        if a >= b {
            return;
        }
        let v = match kind {
            Mark::Em => &mut self.em,
            Mark::Strong => &mut self.strong,
            Mark::Strike => &mut self.strike,
            Mark::Link => &mut self.link,
        };
        v[a] += 1;
        v[b] -= 1;
    }
}

fn push_node(nodes: &mut Vec<Node>, marks: &mut Marks, n: Node) -> usize {
    nodes.push(n);
    marks.grow();
    nodes.len() - 1
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

fn run_len(ch: &[char], i: usize, c: char) -> usize {
    ch[i..].iter().take_while(|x| **x == c).count()
}

/// Pairs the delimiters `delims[range]` into emphasis, strong and strike (the CommonMark algorithm: a closer looks back for an opener,
/// and what it passes over is not matched again), and marks the nodes between each pair.
fn process_emphasis(delims: &mut [Delim], lo: usize, hi: usize, marks: &mut Marks) {
    let idxs: Vec<usize> = (lo..hi).filter(|i| delims[*i].live).collect();
    let m = idxs.len();
    let mut prev: Vec<Option<usize>> = (0..m).map(|k| k.checked_sub(1)).collect();
    let mut next: Vec<Option<usize>> = (0..m).map(|k| (k + 1 < m).then_some(k + 1)).collect();
    let mut bottoms: HashMap<(char, bool, usize), Option<usize>> = HashMap::new();
    let unlink = |x: usize, prev: &mut Vec<Option<usize>>, next: &mut Vec<Option<usize>>| {
        let (p, n) = (prev[x], next[x]);
        if let Some(p) = p {
            next[p] = n;
        }
        if let Some(n) = n {
            prev[n] = p;
        }
    };
    let mut cur = (m > 0).then_some(0);
    while let Some(c) = cur {
        let (ch, orig, can_open, can_close) = {
            let d = &delims[idxs[c]];
            (d.ch, d.orig, d.can_open, d.can_close)
        };
        if !can_close {
            cur = next[c];
            continue;
        }
        let key = (ch, can_open, orig % 3);
        let bottom = bottoms.get(&key).copied().flatten();
        let mut o = prev[c];
        let mut found = None;
        while let Some(oi) = o {
            if Some(oi) == bottom {
                break;
            }
            let od = &delims[idxs[oi]];
            if od.ch == ch && od.can_open {
                let odd = (can_open || od.can_close)
                    && !orig.is_multiple_of(3)
                    && (od.orig + orig).is_multiple_of(3);
                if !odd {
                    found = Some(oi);
                    break;
                }
            }
            o = prev[oi];
        }
        let Some(oi) = found else {
            bottoms.insert(key, prev[c]);
            let after = next[c];
            if !can_open {
                unlink(c, &mut prev, &mut next);
            }
            cur = after;
            continue;
        };
        let (on, cn) = (delims[idxs[oi]].count, delims[idxs[c]].count);
        let strong = ch == '~' || (on >= 2 && cn >= 2);
        let used = if strong { 2 } else { 1 };
        let (a, b) = (delims[idxs[oi]].node, delims[idxs[c]].node);
        let kind = match (ch, strong) {
            ('~', _) => Mark::Strike,
            (_, true) => Mark::Strong,
            _ => Mark::Em,
        };
        marks.add(kind, a + 1, b);
        delims[idxs[oi]].count = on.saturating_sub(used);
        delims[idxs[c]].count = cn.saturating_sub(used);
        // What lay between the pair is never matched again.
        next[oi] = Some(c);
        prev[c] = Some(oi);
        if delims[idxs[oi]].count == 0 {
            unlink(oi, &mut prev, &mut next);
        }
        if delims[idxs[c]].count == 0 {
            let after = next[c];
            unlink(c, &mut prev, &mut next);
            cur = after;
        }
    }
    for i in idxs {
        delims[i].live = false;
    }
}

/// A link destination after `](`: `(` is at `start - 1`. Returns the destination and the index after the closing `)`.
fn parse_dest(ch: &[char], start: usize, budget: &mut u64) -> Option<(String, usize)> {
    let n = ch.len();
    let mut i = start;
    let skip_ws = |i: &mut usize, budget: &mut u64| {
        while *i < n && ch[*i].is_whitespace() && *budget > 0 {
            *i += 1;
            *budget -= 1;
        }
    };
    skip_ws(&mut i, budget);
    let mut dest = String::new();
    if ch.get(i) == Some(&'<') {
        i += 1;
        loop {
            if *budget == 0 || i >= n || ch[i] == '\n' || ch[i] == '<' {
                return None;
            }
            *budget -= 1;
            if ch[i] == '>' {
                i += 1;
                break;
            }
            dest.push(ch[i]);
            i += 1;
        }
    } else {
        let mut depth = 0usize;
        while i < n && *budget > 0 {
            let c = ch[i];
            if c.is_whitespace() {
                break;
            }
            if c.is_control() {
                return None;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            *budget -= 1;
            dest.push(c);
            i += 1;
        }
        if depth != 0 {
            return None;
        }
    }
    let dest_end = i;
    skip_ws(&mut i, budget);
    if let Some(&q) = ch.get(i)
        && matches!(q, '"' | '\'' | '(')
        && i > dest_end
    {
        let close = if q == '(' { ')' } else { q };
        i += 1;
        loop {
            if *budget == 0 || i >= n {
                return None;
            }
            *budget -= 1;
            if ch[i] == '\\' {
                i += 2;
                continue;
            }
            if ch[i] == close {
                i += 1;
                break;
            }
            i += 1;
        }
        skip_ws(&mut i, budget);
    }
    (ch.get(i) == Some(&')')).then_some((dest, i + 1))
}

fn autolink_end(ch: &[char], i: usize) -> Option<usize> {
    // `<scheme:rest>` with no space or `<` inside.
    let mut j = i + 1;
    let limit = (i + 2048).min(ch.len());
    while j < limit && ch[j] != '>' {
        if ch[j].is_whitespace() || ch[j] == '<' {
            return None;
        }
        j += 1;
    }
    if j >= limit || ch.get(j) != Some(&'>') {
        return None;
    }
    let inner: String = ch[i + 1..j].iter().collect();
    let (scheme, rest) = inner.split_once(':')?;
    let ok = (2..=32).contains(&scheme.len())
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && !rest.is_empty();
    ok.then_some(j)
}

/// The styled units of one paragraph, heading or table cell. `base` is the style of the block.
fn inline(text: &str, base: Style, work: &mut u64) -> Vec<Tok> {
    let ch: Vec<char> = text.chars().collect();
    let n = ch.len();
    let mut nodes: Vec<Node> = Vec::new();
    let mut delims: Vec<Delim> = Vec::new();
    let mut marks = Marks::new();
    let mut buf = String::new();
    let mut budget: u64 = 64 * n as u64 + 256;
    // The maximal runs of backticks, by length: a code span closes at the next run of its own length.
    let mut runs: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut i = 0;
    while i < n {
        if ch[i] == '`' {
            let k = run_len(&ch, i, '`');
            runs.entry(k).or_default().push(i);
            i += k;
        } else {
            i += 1;
        }
    }
    struct Bracket {
        node: usize,
        image: bool,
        delims_at: usize,
    }
    let mut brackets: Vec<Bracket> = Vec::new();
    let flush = |buf: &mut String, nodes: &mut Vec<Node>, marks: &mut Marks| {
        if !buf.is_empty() {
            push_node(nodes, marks, Node::Text(std::mem::take(buf)));
        }
    };
    let mut i = 0;
    while i < n {
        *work += 1;
        let c = ch[i];
        match c {
            '\\' => match ch.get(i + 1) {
                Some(&p) if p.is_ascii_punctuation() => {
                    buf.push(p);
                    i += 2;
                }
                Some('\n') => {
                    flush(&mut buf, &mut nodes, &mut marks);
                    push_node(&mut nodes, &mut marks, Node::Break);
                    i += 2;
                }
                _ => {
                    buf.push('\\');
                    i += 1;
                }
            },
            '`' => {
                let k = run_len(&ch, i, '`');
                let closer = runs.get(&k).and_then(|v| {
                    let p = v.partition_point(|s| *s < i + k);
                    v.get(p).copied()
                });
                match closer {
                    Some(j) => {
                        flush(&mut buf, &mut nodes, &mut marks);
                        *work += (j - i) as u64;
                        let mut s: String = ch[i + k..j]
                            .iter()
                            .map(|c| if *c == '\n' { ' ' } else { *c })
                            .collect();
                        if s.starts_with(' ') && s.ends_with(' ') && !s.trim_matches(' ').is_empty()
                        {
                            s = s[1..s.len() - 1].to_string();
                        }
                        push_node(&mut nodes, &mut marks, Node::Code(s));
                        i = j + k;
                    }
                    None => {
                        buf.extend(std::iter::repeat_n('`', k));
                        i += k;
                    }
                }
            }
            '<' => match autolink_end(&ch, i) {
                Some(j) => {
                    flush(&mut buf, &mut nodes, &mut marks);
                    let url: String = ch[i + 1..j].iter().collect();
                    push_node(&mut nodes, &mut marks, Node::Text(url));
                    *work += (j - i) as u64;
                    i = j + 1;
                }
                None => {
                    buf.push('<');
                    i += 1;
                }
            },
            '[' | '!' if c == '[' || ch.get(i + 1) == Some(&'[') => {
                flush(&mut buf, &mut nodes, &mut marks);
                let image = c == '!';
                let node = push_node(
                    &mut nodes,
                    &mut marks,
                    Node::Text(if image { "![" } else { "[" }.to_string()),
                );
                brackets.push(Bracket {
                    node,
                    image,
                    delims_at: delims.len(),
                });
                i += if image { 2 } else { 1 };
            }
            ']' => {
                flush(&mut buf, &mut nodes, &mut marks);
                let Some(b) = brackets.pop() else {
                    buf.push(']');
                    i += 1;
                    continue;
                };
                let dest = if ch.get(i + 1) == Some(&'(') {
                    parse_dest(&ch, i + 2, &mut budget)
                } else {
                    None
                };
                let Some((url, end)) = dest else {
                    buf.push(']');
                    i += 1;
                    continue;
                };
                let hi = delims.len();
                process_emphasis(&mut delims, b.delims_at, hi, &mut marks);
                if b.image {
                    let alt: String = nodes[b.node + 1..]
                        .iter()
                        .filter_map(|nd| match nd {
                            Node::Text(t) | Node::Code(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect();
                    nodes.truncate(b.node);
                    marks.truncate(b.node);
                    delims.truncate(b.delims_at);
                    push_node(
                        &mut nodes,
                        &mut marks,
                        Node::Text(format!("[image: {alt}]")),
                    );
                } else {
                    nodes[b.node] = Node::Text(String::new());
                    let len = nodes.len();
                    marks.add(Mark::Link, b.node + 1, len);
                }
                let tail = if url.is_empty() {
                    String::new()
                } else {
                    format!(" ({url})")
                };
                push_node(&mut nodes, &mut marks, Node::Tail(tail));
                *work += (end - i) as u64;
                i = end;
            }
            '*' | '_' | '~' => {
                let k = run_len(&ch, i, c);
                if c == '~' && k != 2 {
                    buf.extend(std::iter::repeat_n('~', k));
                    i += k;
                    continue;
                }
                flush(&mut buf, &mut nodes, &mut marks);
                let before = if i == 0 { ' ' } else { ch[i - 1] };
                let after = ch.get(i + k).copied().unwrap_or(' ');
                let left = !after.is_whitespace()
                    && (!is_punct(after) || before.is_whitespace() || is_punct(before));
                let right = !before.is_whitespace()
                    && (!is_punct(before) || after.is_whitespace() || is_punct(after));
                let (can_open, can_close) = if c == '_' {
                    (
                        left && (!right || is_punct(before)),
                        right && (!left || is_punct(after)),
                    )
                } else {
                    (left, right)
                };
                let d = delims.len();
                let node = push_node(&mut nodes, &mut marks, Node::Delim(d));
                delims.push(Delim {
                    ch: c,
                    orig: k,
                    count: k,
                    can_open,
                    can_close,
                    node,
                    live: true,
                });
                i += k;
            }
            '\n' => {
                while buf.ends_with(' ') {
                    buf.pop();
                }
                flush(&mut buf, &mut nodes, &mut marks);
                push_node(&mut nodes, &mut marks, Node::Break);
                i += 1;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut nodes, &mut marks);
    let hi = delims.len();
    process_emphasis(&mut delims, 0, hi, &mut marks);
    // Styles, from the sums of the marks.
    let (mut em, mut strong, mut strike, mut link) = (0i32, 0i32, 0i32, 0i32);
    let mut toks: Vec<Tok> = Vec::new();
    for (idx, node) in nodes.iter().enumerate() {
        em += marks.em[idx];
        strong += marks.strong[idx];
        strike += marks.strike[idx];
        link += marks.link[idx];
        let mut st = base;
        if em > 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if strong > 0 {
            st = st.add_modifier(Modifier::BOLD);
        }
        if strike > 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        if link > 0 {
            st = st.add_modifier(Modifier::UNDERLINED);
        }
        match node {
            Node::Text(t) => toks.extend(units_of(t, st, true).into_iter().map(Tok::U)),
            Node::Code(t) => toks.extend(
                units_of(t, st.patch(code_style()), false)
                    .into_iter()
                    .map(Tok::U),
            ),
            Node::Delim(d) => {
                let left = delims[*d].ch.to_string().repeat(delims[*d].count);
                toks.extend(units_of(&left, st, true).into_iter().map(Tok::U));
            }
            Node::Tail(t) => {
                toks.extend(units_of(t, base.patch(dim()), true).into_iter().map(Tok::U))
            }
            Node::Break => toks.push(Tok::Break),
        }
    }
    toks
}

// ---- blocks -----------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Right,
    Center,
}

#[derive(Debug)]
struct Table {
    aligns: Vec<Align>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

#[derive(Debug)]
struct Item {
    /// The marker as written (`1.`) for an ordered list, empty for a bullet.
    marker: String,
    blocks: Vec<Blk>,
}

#[derive(Debug)]
enum Blk {
    Para(String),
    Heading(usize, String),
    Rule,
    Code(Vec<String>),
    Quote(Vec<Blk>, String),
    List(Vec<Item>, String),
    Table(Table, String),
}

fn indent_of(line: &str) -> usize {
    line.bytes().take_while(|b| *b == b' ').count()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn fence_open(line: &str) -> Option<(char, usize, usize)> {
    let ind = indent_of(line);
    if ind >= 4 {
        return None;
    }
    let t = &line[ind..];
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = t.chars().take_while(|x| *x == c).count();
    if run < 3 || (c == '`' && t[run..].contains('`')) {
        return None;
    }
    Some((c, run, ind))
}

fn fence_closes(line: &str, c: char, len: usize) -> bool {
    let ind = indent_of(line);
    if ind >= 4 {
        return false;
    }
    let t = &line[ind..];
    let run = t.chars().take_while(|x| *x == c).count();
    run >= len && t[run..].trim().is_empty()
}

fn atx(line: &str) -> Option<(usize, String)> {
    let ind = indent_of(line);
    if ind >= 4 {
        return None;
    }
    let t = &line[ind..];
    let level = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    let mut text = rest.trim().to_string();
    // A closing run of `#` after a space is not part of the text.
    let stripped = text.trim_end_matches('#');
    if stripped.len() != text.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        text = stripped.trim_end().to_string();
    }
    Some((level, text))
}

fn is_rule(line: &str) -> bool {
    if indent_of(line) >= 4 {
        return false;
    }
    let t: String = line.chars().filter(|c| *c != ' ').collect();
    let Some(c) = t.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    t.chars().count() >= 3 && t.chars().all(|x| x == c)
}

fn quote_strip(line: &str) -> Option<String> {
    let ind = indent_of(line);
    if ind >= 4 {
        return None;
    }
    let rest = line[ind..].strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

struct Marker {
    ordered: bool,
    text: String,
    content_indent: usize,
    rest: String,
    number: Option<u64>,
}

fn list_marker(line: &str) -> Option<Marker> {
    let ind = indent_of(line);
    if ind >= 4 {
        return None;
    }
    let t = &line[ind..];
    let (ordered, text, number): (bool, String, Option<u64>) = if let Some(c) =
        t.chars().next().filter(|c| matches!(c, '-' | '+' | '*'))
    {
        (false, c.to_string(), None)
    } else {
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&digits) || !matches!(t.as_bytes().get(digits), Some(b'.' | b')')) {
            return None;
        }
        (true, t[..=digits].to_string(), t[..digits].parse().ok())
    };
    let after = &t[text.len()..];
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let spaces = after.bytes().take_while(|b| *b == b' ').count();
    let blank_rest = after.trim().is_empty();
    // A blank rest, or five spaces or more (indented code), puts the content one space after the marker.
    let skip = if blank_rest || spaces > 4 {
        spaces.min(1)
    } else {
        spaces
    };
    let rest = if blank_rest {
        String::new()
    } else {
        after[skip..].to_string()
    };
    Some(Marker {
        ordered,
        content_indent: ind + text.len() + skip.max(1),
        text,
        rest,
        number,
    })
}

/// Whether a list marker may interrupt a paragraph: a bullet or `1.` with something after it.
fn interrupts(m: &Marker) -> bool {
    !m.rest.trim().is_empty() && (!m.ordered || m.number == Some(1))
}

/// Whether `line` starts a block of its own (so it cannot be a lazy continuation of a paragraph).
fn starts_block(line: &str) -> bool {
    fence_open(line).is_some()
        || atx(line).is_some()
        || is_rule(line)
        || quote_strip(line).is_some()
        || list_marker(line).is_some_and(|m| interrupts(&m))
}

/// Whether `line` is plain paragraph text (a later unmarked line may continue it).
fn is_plain_text(line: &str) -> bool {
    !is_blank(line) && fence_open(line).is_none() && atx(line).is_none() && !is_rule(line)
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let chars: Vec<char> = t.chars().collect();
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut first = true;
    while i < chars.len() {
        match chars[i] {
            '\\' if chars.get(i + 1) == Some(&'|') => {
                cur.push_str("\\|");
                i += 2;
            }
            '|' => {
                if !(first && cur.trim().is_empty()) {
                    cells.push(cur.trim().to_string());
                }
                cur.clear();
                first = false;
                i += 1;
            }
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        cells.push(cur.trim().to_string());
    }
    cells
}

fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') || !(line.contains('|') || line.trim().starts_with(':')) {
        return None;
    }
    let cells = split_row(line);
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|c| {
            let left = c.starts_with(':');
            let right = c.ends_with(':') && c.len() > 1;
            let dashes = c.trim_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|x| x == '-')).then_some(
                match (left, right) {
                    (true, true) => Align::Center,
                    (true, false) => Align::Left,
                    (false, true) => Align::Right,
                    (false, false) => Align::None,
                },
            )
        })
        .collect()
}

fn table_at(lines: &[String], i: usize) -> Option<Vec<Align>> {
    let head = lines.get(i)?;
    if !head.contains('|') || is_blank(head) {
        return None;
    }
    let delim = delimiter_row(lines.get(i + 1)?)?;
    (split_row(head).len() == delim.len()).then_some(delim)
}

fn paragraphs_of(lines: &[String]) -> Vec<Blk> {
    let mut out = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    for l in lines {
        if is_blank(l) {
            if !para.is_empty() {
                out.push(Blk::Para(para.join("\n")));
                para.clear();
            }
        } else {
            para.push(l.trim_start());
        }
    }
    if !para.is_empty() {
        out.push(Blk::Para(para.join("\n")));
    }
    out
}

fn parse(lines: &[String], depth: usize, work: &mut u64) -> Vec<Blk> {
    if depth >= MAX_DEPTH {
        return paragraphs_of(lines);
    }
    let n = lines.len();
    let mut out: Vec<Blk> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !para.is_empty() {
                out.push(Blk::Para(para.join("\n")));
                para.clear();
            }
        };
    }
    while i < n {
        *work += 1;
        let line = &lines[i];
        if is_blank(line) {
            flush!();
            i += 1;
            continue;
        }
        if let Some((c, len, ind)) = fence_open(line) {
            flush!();
            let mut code = Vec::new();
            i += 1;
            while i < n && !fence_closes(&lines[i], c, len) {
                *work += 1;
                let l = &lines[i];
                let strip = indent_of(l).min(ind);
                code.push(l[strip..].to_string());
                i += 1;
            }
            i += 1;
            out.push(Blk::Code(code));
            continue;
        }
        if indent_of(line) >= 4 && para.is_empty() {
            let mut code: Vec<String> = Vec::new();
            while i < n && (indent_of(&lines[i]) >= 4 || is_blank(&lines[i])) {
                *work += 1;
                let l = &lines[i];
                code.push(if is_blank(l) {
                    String::new()
                } else {
                    l[4..].to_string()
                });
                i += 1;
            }
            while code.last().is_some_and(String::is_empty) {
                code.pop();
            }
            out.push(Blk::Code(code));
            continue;
        }
        if let Some((level, text)) = atx(line) {
            flush!();
            out.push(Blk::Heading(level, text));
            i += 1;
            continue;
        }
        if is_rule(line) {
            flush!();
            out.push(Blk::Rule);
            i += 1;
            continue;
        }
        if let Some(first) = quote_strip(line) {
            flush!();
            let mut inner = vec![first];
            let mut raw = vec![line.clone()];
            let mut open: Option<(char, usize)> =
                fence_open(inner.last().map_or("", String::as_str)).map(|(c, l, _)| (c, l));
            i += 1;
            while i < n {
                *work += 1;
                let l = &lines[i];
                let step = if let Some(rest) = quote_strip(l) {
                    rest
                } else if open.is_none()
                    && inner.last().is_some_and(|p| is_plain_text(p))
                    && !is_blank(l)
                    && !starts_block(l)
                {
                    l.trim_start().to_string()
                } else {
                    break;
                };
                match open {
                    Some((c, len)) if fence_closes(&step, c, len) => open = None,
                    None => open = fence_open(&step).map(|(c, l, _)| (c, l)),
                    _ => {}
                }
                inner.push(step);
                raw.push(l.clone());
                i += 1;
            }
            let blocks = parse(&inner, depth + 1, work);
            out.push(Blk::Quote(blocks, raw.join("\n")));
            continue;
        }
        if let Some(first) = list_marker(line)
            && (para.is_empty() || interrupts(&first))
        {
            flush!();
            let mut items: Vec<Item> = Vec::new();
            let mut raw: Vec<String> = Vec::new();
            let mut m = first;
            loop {
                let ordered = m.ordered;
                let ci = m.content_indent;
                let marker = if ordered {
                    m.text.clone()
                } else {
                    String::new()
                };
                let mut item_lines = vec![m.rest.clone()];
                raw.push(lines[i].clone());
                let mut open: Option<(char, usize)> = fence_open(&m.rest).map(|(c, l, _)| (c, l));
                let mut j = i + 1;
                while j < n {
                    *work += 1;
                    let l = &lines[j];
                    if is_blank(l) {
                        let k = (j..n).find(|k| !is_blank(&lines[*k]));
                        match k {
                            Some(k) if indent_of(&lines[k]) >= ci => {
                                for blank_line in &lines[j..k] {
                                    item_lines.push(String::new());
                                    raw.push(blank_line.clone());
                                }
                                j = k;
                                continue;
                            }
                            _ => break,
                        }
                    }
                    let step = if indent_of(l) >= ci {
                        l[ci..].to_string()
                    } else if list_marker(l).is_some_and(|s| s.ordered == ordered) && !is_rule(l) {
                        break;
                    } else if open.is_none()
                        && item_lines.last().is_some_and(|p| is_plain_text(p))
                        && !starts_block(l)
                    {
                        l.trim_start().to_string()
                    } else {
                        break;
                    };
                    match open {
                        Some((c, len)) if fence_closes(&step, c, len) => open = None,
                        None => open = fence_open(&step).map(|(c, l, _)| (c, l)),
                        _ => {}
                    }
                    item_lines.push(step);
                    raw.push(l.clone());
                    j += 1;
                }
                while item_lines.last().is_some_and(|l| l.is_empty()) {
                    item_lines.pop();
                }
                let blocks = parse(&item_lines, depth + 1, work);
                items.push(Item { marker, blocks });
                // The next item may come after blank lines.
                let k = (j..n).find(|k| !is_blank(&lines[*k]));
                match k.and_then(|k| list_marker(&lines[k]).map(|s| (k, s))) {
                    Some((k, s)) if s.ordered == ordered && !is_rule(&lines[k]) => {
                        m = s;
                        i = k;
                    }
                    _ => {
                        i = j;
                        break;
                    }
                }
            }
            out.push(Blk::List(items, raw.join("\n")));
            continue;
        }
        if let Some(aligns) = table_at(lines, i) {
            flush!();
            let header = split_row(line);
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut raw = vec![line.clone(), lines[i + 1].clone()];
            i += 2;
            while i < n
                && !is_blank(&lines[i])
                && lines[i].contains('|')
                && !starts_block(&lines[i])
            {
                *work += 1;
                let mut cells = split_row(&lines[i]);
                cells.resize(aligns.len(), String::new());
                rows.push(cells);
                raw.push(lines[i].clone());
                i += 1;
            }
            out.push(Blk::Table(
                Table {
                    aligns,
                    header,
                    rows,
                },
                raw.join("\n"),
            ));
            continue;
        }
        para.push(line.trim_start().to_string());
        i += 1;
    }
    flush!();
    out
}

// ---- rows of blocks ---------------------------------------------------------------------------------------------------------------

fn prefixed(rows: Vec<Row>, first: &str, rest: &str, style: Style) -> Vec<Row> {
    rows.into_iter()
        .enumerate()
        .map(|(k, r)| {
            let p = if k == 0 { first } else { rest };
            let mut spans = Vec::with_capacity(r.line.spans.len() + 1);
            if r.line.spans.is_empty() {
                let t = p.trim_end();
                if !t.is_empty() {
                    spans.push(Span::styled(t.to_string(), style));
                }
            } else {
                if !p.is_empty() {
                    spans.push(Span::styled(p.to_string(), style));
                }
                spans.extend(r.line.spans);
            }
            Row {
                line: Line::from(spans),
                join: r.join,
            }
        })
        .collect()
}

fn bullet(depth: usize) -> &'static str {
    ["\u{2022}", "\u{25e6}", "\u{25aa}"][depth % 3]
}

fn render_table(t: &Table, raw: &str, width: usize, work: &mut u64) -> Vec<Row> {
    let cols = t.aligns.len();
    let fallback = |work: &mut u64| plain_rows(raw, width, Style::default(), work);
    if cols == 0 || cols > MAX_TABLE_COLS {
        return fallback(work);
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cell_toks = |text: &str, base: Style, work: &mut u64| -> Vec<Tok> {
        let text = replace_br(text);
        inline(&text, base, work)
    };
    // Natural width of every column.
    let mut natural = vec![1usize; cols];
    let mut laid: Vec<Vec<Vec<Tok>>> = Vec::new();
    let mut all: Vec<(&Vec<String>, Style)> = vec![(&t.header, bold)];
    all.extend(t.rows.iter().map(|r| (r, Style::default())));
    for (cells, base) in &all {
        let mut row = Vec::new();
        for (c, text) in cells.iter().enumerate().take(cols) {
            let toks = cell_toks(text, *base, work);
            let lines = wrap(toks_clone(&toks), usize::MAX / 4, work);
            let w = lines.iter().map(|(u, _)| rows_width(u)).max().unwrap_or(0);
            natural[c] = natural[c].max(w);
            row.push(toks);
        }
        while row.len() < cols {
            row.push(Vec::new());
        }
        laid.push(row);
    }
    let mut widths = natural;
    let total = |w: &[usize]| w.iter().sum::<usize>() + 3 * cols + 1;
    while total(&widths) > width {
        *work += cols as u64;
        let (c, &wc) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .unwrap_or((0, &0));
        if wc <= 3 {
            return fallback(work);
        }
        let second = widths
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != c)
            .map(|(_, w)| *w)
            .max()
            .unwrap_or(0)
            .max(3);
        let need = total(&widths) - width;
        let cut = (wc - second).clamp(1, need.max(1));
        widths[c] -= cut.min(wc - 3);
    }
    let mut rows: Vec<Row> = Vec::new();
    for (ri, row) in laid.into_iter().enumerate() {
        let wrapped: Vec<Vec<(Vec<Unit>, Join)>> = row
            .into_iter()
            .enumerate()
            .map(|(c, toks)| wrap(toks, widths[c], work))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for li in 0..height {
            let mut units: Vec<Unit> = units_of("| ", Style::default(), false);
            for (c, (col_lines, width_c)) in wrapped.iter().zip(&widths).enumerate() {
                let cell: &[Unit] = col_lines.get(li).map_or(&[][..], |(u, _)| u.as_slice());
                let pad = width_c.saturating_sub(rows_width(cell));
                let (l, r) = match t.aligns[c] {
                    Align::Right => (pad, 0),
                    Align::Center => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                units.extend(units_of(&" ".repeat(l), Style::default(), false));
                units.extend(cell.iter().cloned());
                units.extend(units_of(&" ".repeat(r), Style::default(), false));
                units.extend(units_of(
                    if c + 1 < cols { " | " } else { " |" },
                    Style::default(),
                    false,
                ));
            }
            rows.push(row_of(&units, Join::Hard));
        }
        if ri == 0 {
            let mut d = String::from("|");
            for (w, align) in widths.iter().zip(&t.aligns) {
                let w = *w;
                d.push_str(&match align {
                    Align::Left => format!(":{}", "-".repeat(w + 1)),
                    Align::Right => format!("{}:", "-".repeat(w + 1)),
                    Align::Center => format!(":{}:", "-".repeat(w)),
                    Align::None => "-".repeat(w + 2),
                });
                d.push('|');
            }
            rows.push(row_of(&units_of(&d, dim(), false), Join::Hard));
        }
    }
    rows
}

fn toks_clone(toks: &[Tok]) -> Vec<Tok> {
    toks.iter()
        .map(|t| match t {
            Tok::U(u) => Tok::U(u.clone()),
            Tok::Break => Tok::Break,
        })
        .collect()
}

fn replace_br(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &lower[i..];
        let len = ["<br>", "<br/>", "<br />"]
            .iter()
            .find(|p| rest.starts_with(**p))
            .map(|p| p.len());
        match len {
            Some(l) => {
                out.push('\n');
                i += l;
            }
            None => {
                let c = s[i..].chars().next().unwrap_or(' ');
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

fn render_one(b: &Blk, width: usize, list_depth: usize, work: &mut u64) -> Vec<Row> {
    match b {
        Blk::Para(text) => {
            let toks = inline(text, Style::default(), work);
            wrapped_rows(toks, width, work)
        }
        Blk::Heading(level, text) => {
            let mut base = Style::default().add_modifier(Modifier::BOLD);
            if *level == 1 {
                base = base.add_modifier(Modifier::UNDERLINED);
            }
            let toks = inline(text, base, work);
            wrapped_rows(toks, width, work)
        }
        Blk::Rule => vec![row_of(
            &units_of(&"\u{2500}".repeat(width.min(RULE_MAX)), dim(), false),
            Join::Hard,
        )],
        Blk::Code(lines) => {
            let mut rows = Vec::new();
            for l in lines {
                let units = units_of(&expand_tabs(l), code_style(), false);
                if units.is_empty() {
                    rows.push(blank());
                } else {
                    rows.extend(cut_rows(units, width, work));
                }
            }
            rows
        }
        Blk::Quote(inner, raw) => {
            if width < 3 {
                return plain_rows(raw, width, Style::default(), work);
            }
            let rows = render_blocks(inner, width - 2, list_depth, false, work);
            prefixed(rows, "\u{2502} ", "\u{2502} ", dim())
        }
        Blk::List(items, raw) => {
            let mut rows = Vec::new();
            for it in items {
                let marker = if it.marker.is_empty() {
                    bullet(list_depth).to_string()
                } else {
                    it.marker.clone()
                };
                let mw = display_units(&marker).iter().map(|u| u.2).sum::<usize>() + 1;
                if width <= mw {
                    return plain_rows(raw, width, Style::default(), work);
                }
                let content = render_blocks(&it.blocks, width - mw, list_depth + 1, true, work);
                if content.is_empty() {
                    rows.push(row_of(
                        &units_of(&marker, Style::default(), false),
                        Join::Hard,
                    ));
                } else {
                    rows.extend(prefixed(
                        content,
                        &format!("{marker} "),
                        &" ".repeat(mw),
                        Style::default(),
                    ));
                }
            }
            rows
        }
        Blk::Table(t, raw) => render_table(t, raw, width, work),
    }
}

fn render_blocks(
    blocks: &[Blk],
    width: usize,
    list_depth: usize,
    tight: bool,
    work: &mut u64,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut prev: Option<&Blk> = None;
    for b in blocks {
        let r = render_one(b, width, list_depth, work);
        if r.is_empty() {
            continue;
        }
        let gap = match (prev, b) {
            (None, _) => false,
            _ if !tight => true,
            (Some(Blk::Para(_)), Blk::Para(_)) => true,
            _ => false,
        };
        if gap {
            rows.push(blank());
        }
        *work += r.len() as u64;
        rows.extend(r);
        prev = Some(b);
    }
    rows
}

fn cap_rows(mut rows: Vec<Row>) -> Vec<Row> {
    if rows.len() > MAX_ROWS {
        let more = rows.len() - MAX_ROWS;
        rows.truncate(MAX_ROWS);
        rows.push(row_of(
            &units_of(
                &format!("[\u{2026} {more} more lines not shown]"),
                dim(),
                false,
            ),
            Join::Hard,
        ));
    }
    rows
}

fn clean(text: &str) -> String {
    display_text(text)
        .split('\n')
        .map(expand_tabs)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders Markdown into rows of at most `width` cells, with the number of steps taken (for tests that bound the cost).
pub fn render_counted(text: &str, width: usize) -> (Vec<Row>, u64) {
    let mut work = 0u64;
    if width == 0 {
        return (Vec::new(), work);
    }
    let cleaned = clean(text);
    let lines: Vec<String> = cleaned.split('\n').map(str::to_string).collect();
    work += lines.len() as u64 + cleaned.len() as u64;
    let blocks = parse(&lines, 0, &mut work);
    let rows = render_blocks(&blocks, width, 0, false, &mut work);
    (cap_rows(rows), work)
}

/// Renders Markdown into rows of at most `width` cells.
pub fn render(text: &str, width: usize) -> Vec<Row> {
    render_counted(text, width).0
}

// ---- diffs ------------------------------------------------------------------------------------------------------------------------

/// Whether `text` is a unified diff: a hunk header (`@@ -a,b +c,d @@`) or a `---` line followed by a `+++` line.
pub fn is_unified_diff(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .any(|l| l.starts_with("@@ -") && l[2..].contains(" +"))
        || lines
            .windows(2)
            .any(|w| w[0].starts_with("--- ") && w[1].starts_with("+++ "))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dk {
    Header,
    Hunk,
    Context,
    Removed,
    Added,
    Note,
    Plain,
}

fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    // `@@ -a[,b] +c[,d] @@`
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let len = |s: &str| -> Option<usize> {
        match s.split_once(',') {
            Some((_, l)) => l.parse().ok(),
            None => s.parse::<usize>().ok().map(|_| 1),
        }
    };
    Some((len(old)?, len(new)?))
}

fn is_file_header(line: &str) -> bool {
    [
        "diff ",
        "index ",
        "--- ",
        "+++ ",
        "new file",
        "deleted file",
        "similarity ",
        "rename ",
        "Binary files",
        "old mode",
        "new mode",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

fn classify(lines: &[&str]) -> Vec<Dk> {
    let mut out = Vec::with_capacity(lines.len());
    let (mut old_left, mut new_left) = (0usize, 0usize);
    let mut unbounded = false;
    for (i, l) in lines.iter().enumerate() {
        let in_hunk = old_left > 0 || new_left > 0 || unbounded;
        let ends_hunk = unbounded
            && (l.starts_with("diff ")
                || (l.starts_with("--- ")
                    && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))));
        if l.starts_with("@@ ") || ends_hunk {
            if l.starts_with("@@ ") {
                match hunk_counts(l) {
                    Some((o, n)) => {
                        old_left = o;
                        new_left = n;
                        unbounded = false;
                    }
                    None => {
                        old_left = 0;
                        new_left = 0;
                        unbounded = true;
                    }
                }
                out.push(Dk::Hunk);
                continue;
            }
            unbounded = false;
            old_left = 0;
            new_left = 0;
        }
        let in_hunk = in_hunk && !ends_hunk;
        if in_hunk {
            match l.chars().next() {
                Some('-') => {
                    old_left = old_left.saturating_sub(1);
                    out.push(Dk::Removed);
                }
                Some('+') => {
                    new_left = new_left.saturating_sub(1);
                    out.push(Dk::Added);
                }
                Some('\\') => out.push(Dk::Note),
                Some(' ') | None => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    out.push(Dk::Context);
                }
                Some(_) => out.push(Dk::Plain),
            }
        } else if is_file_header(l) {
            out.push(Dk::Header);
        } else if l.starts_with('\\') {
            out.push(Dk::Note);
        } else {
            out.push(Dk::Plain);
        }
    }
    out
}

fn diff_tokens(s: &str) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    let mut kind = 0u8; // 1 word, 2 space
    for c in s.chars() {
        let k = if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            0
        };
        if k != 0 && k == kind {
            if let Some(t) = toks.last_mut() {
                t.push(c);
            }
        } else {
            toks.push(c.to_string());
        }
        kind = k;
    }
    toks
}

/// Which tokens of `a` and of `b` are not in their longest common subsequence.
fn changed_tokens(a: &[String], b: &[String]) -> (Vec<bool>, Vec<bool>) {
    let (n, m) = (a.len(), b.len());
    let mut t = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[at(i, j)] = if a[i] == b[j] {
                t[at(i + 1, j + 1)] + 1
            } else {
                t[at(i + 1, j)].max(t[at(i, j + 1)])
            };
        }
    }
    let (mut ca, mut cb) = (vec![true; n], vec![true; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ca[i] = false;
            cb[j] = false;
            i += 1;
            j += 1;
        } else if t[at(i + 1, j)] >= t[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (ca, cb)
}

fn diff_units(line: &str, base: Style, changed: Option<(&[String], &[bool])>) -> Vec<Unit> {
    let Some((tokens, flags)) = changed else {
        return units_of(line, base, false);
    };
    let mut units = units_of(&line[..1], base, false);
    for (tok, f) in tokens.iter().zip(flags) {
        let st = if *f {
            base.add_modifier(Modifier::REVERSED)
        } else {
            base
        };
        units.extend(units_of(tok, st, false));
    }
    units
}

/// Renders a unified diff into rows: file headers bold, hunk headers cyan, removed lines red, added lines green, a note dim, and the words
/// that changed between a removed line and the added line paired with it reversed. Text that is not a unified diff is shown raw.
pub fn render_diff(text: &str, width: usize) -> Vec<Row> {
    let mut work = 0u64;
    if width == 0 {
        return Vec::new();
    }
    let cleaned = clean(text);
    if !is_unified_diff(&cleaned) {
        return cap_rows(plain_rows(&cleaned, width, Style::default(), &mut work));
    }
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let kinds = classify(&lines);
    // Pair the lines of each change: removed lines followed by added lines.
    let mut tokens: HashMap<usize, (Vec<String>, Vec<bool>)> = HashMap::new();
    let mut budget = DIFF_BUDGET;
    let mut i = 0;
    while i < lines.len() {
        if kinds[i] != Dk::Removed {
            i += 1;
            continue;
        }
        let r0 = i;
        while i < lines.len() && kinds[i] == Dk::Removed {
            i += 1;
        }
        let a0 = i;
        while i < lines.len() && kinds[i] == Dk::Added {
            i += 1;
        }
        for k in 0..(a0 - r0).min(i - a0) {
            let (ri, ai) = (r0 + k, a0 + k);
            let (ta, tb) = (diff_tokens(&lines[ri][1..]), diff_tokens(&lines[ai][1..]));
            if ta.len() > DIFF_TOKENS_MAX || tb.len() > DIFF_TOKENS_MAX {
                continue;
            }
            let cost = (ta.len() + 1) * (tb.len() + 1);
            if cost > budget {
                continue;
            }
            budget -= cost;
            let (ca, cb) = changed_tokens(&ta, &tb);
            tokens.insert(ri, (ta, ca));
            tokens.insert(ai, (tb, cb));
        }
    }
    let mut rows = Vec::new();
    for (idx, (l, k)) in lines.iter().zip(&kinds).enumerate() {
        let base = match k {
            Dk::Header => Style::default().add_modifier(Modifier::BOLD),
            Dk::Hunk => Style::default().fg(Color::Cyan),
            Dk::Removed => Style::default().fg(Color::Red),
            Dk::Added => Style::default().fg(Color::Green),
            Dk::Note => dim(),
            Dk::Context | Dk::Plain => Style::default(),
        };
        let changed = tokens.get(&idx).map(|(t, f)| (t.as_slice(), f.as_slice()));
        let units = diff_units(l, base, changed);
        if units.is_empty() {
            rows.push(blank());
        } else {
            rows.extend(cut_rows(units, width, &mut work));
        }
    }
    cap_rows(rows)
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.strip_suffix('\n').unwrap_or(s).split('\n').collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// The edit script between `a` and `b` (lines).
fn edits(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops = vec![Op::Keep; pre];
    if ma.len().saturating_mul(mb.len()) > 1_000_000 {
        ops.extend(std::iter::repeat_n(Op::Del, ma.len()));
        ops.extend(std::iter::repeat_n(Op::Add, mb.len()));
    } else {
        let (n, m) = (ma.len(), mb.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        let at = |i: usize, j: usize| i * (m + 1) + j;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[at(i, j)] = if ma[i] == mb[j] {
                    t[at(i + 1, j + 1)] + 1
                } else {
                    t[at(i + 1, j)].max(t[at(i, j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && ma[i] == mb[j] {
                ops.push(Op::Keep);
                i += 1;
                j += 1;
            } else if i < n && (j == m || t[at(i + 1, j)] >= t[at(i, j + 1)]) {
                ops.push(Op::Del);
                i += 1;
            } else {
                ops.push(Op::Add);
                j += 1;
            }
        }
    }
    ops.extend(std::iter::repeat_n(Op::Keep, suf));
    ops
}

/// A unified diff of `old` and `new` (as lines) with three lines of context; empty when they are equal.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    const CONTEXT: usize = 3;
    let (a, b) = (split_lines(old), split_lines(new));
    let ops = edits(&a, &b);
    if ops.iter().all(|o| *o == Op::Keep) {
        return String::new();
    }
    // Positions in `ops` of the changes, grouped when the gap between them is at most twice the context.
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| **o != Op::Keep)
        .map(|(i, _)| i)
        .collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &c in &changes {
        match groups.last_mut() {
            Some((_, end)) if c - *end <= 2 * CONTEXT + 1 => *end = c,
            _ => groups.push((c, c)),
        }
    }
    // Line numbers at each op: how many old and new lines come before it.
    let mut before: Vec<(usize, usize)> = Vec::with_capacity(ops.len() + 1);
    let (mut oi, mut ni) = (0, 0);
    for o in &ops {
        before.push((oi, ni));
        match o {
            Op::Keep => {
                oi += 1;
                ni += 1;
            }
            Op::Del => oi += 1,
            Op::Add => ni += 1,
        }
    }
    before.push((oi, ni));
    let range = |start0: usize, len: usize| {
        let shown = if len == 0 { start0 } else { start0 + 1 };
        if len == 1 {
            format!("{shown}")
        } else {
            format!("{shown},{len}")
        }
    };
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for (first, last) in groups {
        let from = first.saturating_sub(CONTEXT);
        let to = (last + CONTEXT + 1).min(ops.len());
        let (o0, n0) = before[from];
        let (o1, n1) = before[to];
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(o0, o1 - o0),
            range(n0, n1 - n0)
        ));
        for k in from..to {
            let (oi, ni) = before[k];
            match ops[k] {
                Op::Keep => out.push_str(&format!(" {}\n", a[oi])),
                Op::Del => out.push_str(&format!("-{}\n", a[oi])),
                Op::Add => out.push_str(&format!("+{}\n", b[ni])),
            }
        }
    }
    out
}

// ---- blocks of the transcript -----------------------------------------------------------------------------------------------------

/// The body of a block as rows. An engine message is Markdown; a diff block is a diff when its text is one; thoughts, usage lines and
/// gaps are dim text; everything else (the user's words, tool output, notices, cards) is the text as written. The head line of a block
/// is the transcript's own and is not drawn here.
pub fn render_block(block: &Block, width: usize) -> Vec<Row> {
    let mut work = 0u64;
    if width == 0 {
        return Vec::new();
    }
    match (&block.kind, block.effective_role()) {
        (BlockKind::Diff, _) => render_diff(&block.body, width),
        (BlockKind::Message, Role::Engine) => render(&block.body, width),
        (BlockKind::Thought | BlockKind::Usage | BlockKind::Gap(_), _) => {
            cap_rows(plain_rows(&clean(&block.body), width, dim(), &mut work))
        }
        _ => cap_rows(plain_rows(
            &clean(&block.body),
            width,
            Style::default(),
            &mut work,
        )),
    }
}
