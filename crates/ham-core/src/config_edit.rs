//! Comment-keeping edits of a TOML file, built on the byte spans that `toml::de::DeTable::parse` returns.
//!
//! [`apply`] takes the text of a file and a list of [`Edit`]s and returns the new text, or says that nothing changes, or refuses. It
//! replaces the value span of an existing key, inserts one line for a new key, removes the line of an unset key and leaves every other
//! byte alone: comments, blank lines, order, quoting and the file's own line endings. It knows no key names; the caller passes the path of
//! each key as the file spells it and a `rank` that orders new top-level keys (the registry supplies it).
//!
//! The rules:
//!
//! 1. **Scope.** A scalar or an array at a top-level key, or at a key of a standard `[table]`. A key defined by dotted keys, an inline
//!    table, an array of tables, or a path that runs into a value or such a construct is [`Refusal::Unsupported`] with the line, and nothing
//!    changes. These constructs elsewhere in the file do not stop an edit of a plain key.
//! 2. **No-op.** Setting the value a key already has, or clearing a key that is not there, writes nothing ([`Outcome::Unchanged`]). A value
//!    is the same value when it reads the same (`'x'` and `"x"`, `0.90` and `0.9`, `0x10` and `16`); floats are compared by their bits, so `-0.0` and
//!    `0.0` differ.
//! 3. **Unset removes the key**: its line (all lines of a multi-line value) and its trailing comment; comment lines stay. A table left
//!    with no key and no comment goes with its header and the blank line before it (when no blank line stands above the header, the first
//!    blank line after the table instead).
//! 4. **New keys.** A new top-level key goes before the first existing top-level key that follows it in rank order (a key whose rank is
//!    `usize::MAX` has no place in the registry and never follows), with the comment lines directly above that key staying with it;
//!    else after the last top-level assignment; with none, above the first table (with a blank line after it) or at the end. A new key
//!    in an existing table goes after the last key of that table, or after the header line of an empty table. A new table is appended at the
//!    end of the file after one blank line (a blank line that already ends the file is that line).
//! 5. **Legacy form.** A new key is written as `key = value` on one line, which `setup.sh` reads with `sed` and the Python drivers with a
//!    regex. A key the file already has keeps its own spelling (`default_model` stays `default_model`); the patcher never rewrites a key
//!    name and never creates a second spelling, so the caller passes the spelling it found.
//! 6. **Values.** A string is always a basic double-quoted string with escapes (every control character, line and paragraph separator and
//!    character that the terminal sanitizer marks as invisible is written as `\uXXXX`, or `\UXXXXXXXX` above U+FFFF); a float always has a
//!    fraction or an exponent; an array is written on one line.
//! 7. **Line endings.** The file's own (the first line break decides) are kept; a file without a final newline gets one before appended text.
//! 8. **Bad file.** A file that is not TOML, starts with a byte order mark or holds a NUL is [`Refusal::NotToml`] with the line and column,
//!    never with the source line, and is not touched.
//!
//! Every edit is proven before the next one starts: the new text is read again with `toml::from_str::<toml::Table>`, both trees are
//! flattened, and the new tree must equal the old tree with exactly the intended key changed. If it does not, the call returns
//! [`Refusal::NotProvable`] and the caller keeps its text. A mistake in the patcher can therefore refuse an edit; it cannot write a file
//! that reads differently from what was asked.
//!
//! The `toml` crate does not publish whether a table is dotted, implicit or inline, or whether an array is an array of tables. The patcher
//! reads that from the spans, which the test `tests/config_edit_spans.rs` pins for the exact version of the crate: a standard table's
//! span is its header and starts with `[`; an inline table's starts with `{`; an implicit table has the span of one key segment; an
//! array of tables has the span of its first header, which is also the span of its first element.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use toml::de::{DeTable, DeValue};

/// A value the patcher can write.
#[derive(Clone, Debug, PartialEq)]
pub enum EditValue {
    Bool(bool),
    Int(i64),
    /// Must be finite.
    Float(f64),
    Str(String),
    /// An array of strings, written on one line.
    StrList(Vec<String>),
}

/// One change to a file. `path` is the key as the file spells it: `["region"]` for a top-level key, `["run", "goal"]` for the key `goal`
/// of the standard table `[run]`. A name that holds a dot is one key, not a path.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    Set { path: Vec<String>, value: EditValue },
    Clear { path: Vec<String> },
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No edit changed a byte.
    Unchanged,
    /// The new text of the file.
    Changed(String),
}

/// Why [`apply`] changed nothing. No message carries a line of the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The text is not TOML (also a byte order mark and a NUL). Line and column count from 1; the column counts characters.
    NotToml { line: usize, column: usize },
    /// The edit meets a construct the patcher does not edit. `line` is the line of that construct, 0 when the edit itself is at fault
    /// (an empty path, a float that is not finite).
    Unsupported { line: usize, reason: &'static str },
    /// The patcher built text that does not read as the old tree plus exactly the intended change, so it did not use it.
    NotProvable,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotToml { line, column } => {
                write!(f, "not valid TOML at line {line}, column {column}")
            }
            Self::Unsupported { line: 0, reason } => write!(f, "{reason}"),
            Self::Unsupported { line, reason } => {
                write!(f, "line {line}: {reason}; edit the file by hand")
            }
            Self::NotProvable => f.write_str(
                "the edit could not be shown to change exactly one key; nothing was written",
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Applies `edits` in order, each to the text the one before it produced, and proves each result. `rank` orders new top-level keys: it
/// is called with a one-element path, a lower number sits earlier in the file, and `usize::MAX` means the key has no place in the
/// registry.
pub fn apply(
    text: &str,
    edits: &[Edit],
    rank: &dyn Fn(&[String]) -> usize,
) -> Result<Outcome, Refusal> {
    run(text, edits, rank, 0)
}

/// Test hook of the proof: like [`apply`], but moves the value span of every replaced value by `shift` bytes, so a test can see the
/// proof refuse the damage. Not part of the interface.
#[doc(hidden)]
pub fn apply_with_shifted_value_span(
    text: &str,
    edits: &[Edit],
    rank: &dyn Fn(&[String]) -> usize,
    shift: isize,
) -> Result<Outcome, Refusal> {
    run(text, edits, rank, shift)
}

fn run(
    text: &str,
    edits: &[Edit],
    rank: &dyn Fn(&[String]) -> usize,
    shift: isize,
) -> Result<Outcome, Refusal> {
    check_text(text)?;
    let mut current = text.to_owned();
    for edit in edits {
        if let Some(next) = step(&current, edit, rank, shift)? {
            prove(&current, &next, edit)?;
            current = next;
        }
    }
    Ok(if current == text {
        Outcome::Unchanged
    } else {
        Outcome::Changed(current)
    })
}

/// The refusals that depend on the text alone.
fn check_text(text: &str) -> Result<(), Refusal> {
    if text.starts_with('\u{feff}') {
        return Err(Refusal::NotToml { line: 1, column: 1 });
    }
    if let Some(at) = text.find('\0') {
        let (line, column) = position(text, at);
        return Err(Refusal::NotToml { line, column });
    }
    DeTable::parse(text).map_err(|e| not_toml(text, e.span()))?;
    // Syntax is not enough: a value out of range (an integer over 64 bits) parses here and fails when the tree is built, and the proof
    // builds the tree. The file is not one `ham` can read, so say so with the position instead of failing every edit as not provable.
    toml::from_str::<toml::Table>(text)
        .map(|_| ())
        .map_err(|e| not_toml(text, e.span()))
}

fn not_toml(text: &str, span: Option<Range<usize>>) -> Refusal {
    let (line, column) = position(text, span.map_or(0, |s| s.start));
    Refusal::NotToml { line, column }
}

/// Line and column (from 1, columns in characters) of a byte offset.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = &text[..at];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    (line, column)
}

// ---- one edit ---------------------------------------------------------------------------------------------------------------------

/// The new text for one edit, or `None` when it changes nothing.
fn step(
    text: &str,
    edit: &Edit,
    rank: &dyn Fn(&[String]) -> usize,
    shift: isize,
) -> Result<Option<String>, Refusal> {
    let path = match edit {
        Edit::Set { path, .. } | Edit::Clear { path } => path,
    };
    if path.is_empty() {
        return Err(Refusal::Unsupported {
            line: 0,
            reason: "the key path is empty",
        });
    }
    if let Edit::Set {
        value: EditValue::Float(f),
        ..
    } = edit
        && !f.is_finite()
    {
        return Err(Refusal::Unsupported {
            line: 0,
            reason: "a float must be finite",
        });
    }
    let doc = Doc::parse(text)?;
    match edit {
        Edit::Set { path, value } => doc.set(path, value, rank, shift),
        Edit::Clear { path } => doc.clear(path),
    }
}

// ---- the layout of a file, read from spans ----------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The part of the file above the first header.
    Root,
    /// A standard table, `[a.b]`.
    Header,
    /// One element of an array of tables, `[[a]]`.
    ArrayItem,
}

/// A part of the file that one header (or the start of the file) opens and the next header closes.
#[derive(Debug)]
struct Section {
    path: Vec<String>,
    kind: Kind,
    /// The span of the header text; empty for the root.
    header: Range<usize>,
}

/// What the caller does with the key: `Set` needs a place for it, `Clear` only needs to know whether it is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Set,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A scalar or an array.
    Plain,
    /// An inline table; the patcher edits none.
    Inline,
}

/// A `key = value` of the file.
#[derive(Debug)]
struct Leaf {
    /// The full path from the root of the document.
    path: Vec<String>,
    section: usize,
    /// Where the first segment of the key starts (the key itself unless it is a dotted key).
    lead: usize,
    value: Range<usize>,
    shape: Shape,
    /// Defined with dotted keys (`a.b = 1`), which the patcher does not edit.
    dotted: bool,
}

struct Doc<'a> {
    text: &'a str,
    sections: Vec<Section>,
    /// In file order.
    leaves: Vec<Leaf>,
}

impl<'a> Doc<'a> {
    fn parse(text: &'a str) -> Result<Self, Refusal> {
        let tree = DeTable::parse(text).map_err(|e| not_toml(text, e.span()))?;
        let mut doc = Doc {
            text,
            sections: vec![Section {
                path: Vec::new(),
                kind: Kind::Root,
                header: 0..0,
            }],
            leaves: Vec::new(),
        };
        doc.walk(tree.get_ref(), &mut Vec::new(), 0, None);
        doc.leaves.sort_by_key(|l| l.lead);
        Ok(doc)
    }

    /// Reads the tree. `lead` is the start of the first segment of the dotted key that is being walked, if any.
    fn walk(
        &mut self,
        table: &DeTable<'_>,
        path: &mut Vec<String>,
        section: usize,
        lead: Option<usize>,
    ) {
        for (key, value) in table.iter() {
            path.push(key.get_ref().to_string());
            let span = value.span();
            match value.get_ref() {
                DeValue::Table(inner) => match self.text.as_bytes().get(span.start) {
                    // A standard table: its span is the header.
                    Some(b'[') => {
                        let s = self.add_section(path, Kind::Header, span);
                        self.walk(inner, path, s, None);
                    }
                    Some(b'{') => {
                        let at = lead.unwrap_or_else(|| key.span().start);
                        self.add_leaf(path, section, at, span, Shape::Inline, lead.is_some());
                    }
                    // An implicit table (a dotted key, or the path of a header): its span is one key segment.
                    _ => self.walk(inner, path, section, lead.or(Some(span.start))),
                },
                // An array of tables: the array and its first element share the first header's span.
                DeValue::Array(items) if items.first().is_some_and(|f| f.span() == span) => {
                    for item in items.iter() {
                        if let DeValue::Table(inner) = item.get_ref() {
                            let s = self.add_section(path, Kind::ArrayItem, item.span());
                            self.walk(inner, path, s, None);
                        }
                    }
                }
                _ => {
                    let at = lead.unwrap_or_else(|| key.span().start);
                    self.add_leaf(path, section, at, span, Shape::Plain, lead.is_some());
                }
            }
            path.pop();
        }
    }

    fn add_section(&mut self, path: &[String], kind: Kind, header: Range<usize>) -> usize {
        self.sections.push(Section {
            path: path.to_vec(),
            kind,
            header,
        });
        self.sections.len() - 1
    }

    fn add_leaf(
        &mut self,
        path: &[String],
        section: usize,
        lead: usize,
        value: Range<usize>,
        shape: Shape,
        dotted: bool,
    ) {
        self.leaves.push(Leaf {
            path: path.to_vec(),
            section,
            lead,
            value,
            shape,
            dotted,
        });
    }

    // ---- text helpers ----

    fn slice(&self, range: Range<usize>) -> &'a str {
        self.text.get(range).unwrap_or("")
    }

    /// The file's own line ending: the first line break decides.
    fn eol(&self) -> &'static str {
        match self.text.find('\n') {
            Some(i) if i > 0 && self.text.as_bytes().get(i - 1) == Some(&b'\r') => "\r\n",
            _ => "\n",
        }
    }

    fn line_start(&self, pos: usize) -> usize {
        self.slice(0..pos).rfind('\n').map_or(0, |i| i + 1)
    }

    /// The offset just after the line break that ends the line holding `pos` (the end of the text when there is none).
    fn line_end(&self, pos: usize) -> usize {
        self.slice(pos..self.text.len())
            .find('\n')
            .map_or(self.text.len(), |i| pos + i + 1)
    }

    /// 1-based line number of a byte offset.
    fn line_of(&self, pos: usize) -> usize {
        self.slice(0..pos).matches('\n').count() + 1
    }

    fn unsupported(&self, pos: usize, reason: &'static str) -> Refusal {
        Refusal::Unsupported {
            line: self.line_of(pos),
            reason,
        }
    }

    fn is_blank(&self, range: Range<usize>) -> bool {
        self.slice(range).lines().all(|l| l.trim().is_empty())
    }

    /// The first byte after the line on which the last expression (a header or a `key = value`) before `before` ends. Between that
    /// and the next expression only blank lines and comment lines can stand.
    fn gap_floor(&self, before: usize) -> usize {
        let header_ends = self
            .sections
            .iter()
            .filter(|s| s.kind != Kind::Root)
            .map(|s| s.header.end);
        let value_ends = self.leaves.iter().map(|l| l.value.end);
        header_ends
            .chain(value_ends)
            .filter(|e| *e <= before)
            .max()
            .map_or(0, |e| self.line_end(e))
    }

    /// The start of the comment lines that stand directly above the line starting at `line_start` (no blank line between), or
    /// `line_start` itself when there are none.
    fn attached_start(&self, line_start: usize) -> usize {
        let floor = self.gap_floor(line_start);
        let mut start = line_start;
        while start > floor {
            let prev = self.line_start(start - 1);
            if prev < floor || !self.slice(prev..start).trim_start().starts_with('#') {
                break;
            }
            start = prev;
        }
        start
    }

    /// `text` with `body` inserted at `at`, which is a line start or the end of the text; a text that ends without a line break gets one
    /// first.
    fn insert_at(&self, at: usize, body: &str) -> String {
        let mut out = String::with_capacity(self.text.len() + body.len() + 2);
        out.push_str(self.slice(0..at));
        if at == self.text.len() && !self.text.is_empty() && !self.text.ends_with('\n') {
            out.push_str(self.eol());
        }
        out.push_str(body);
        out.push_str(self.slice(at..self.text.len()));
        out
    }

    fn splice(&self, range: Range<usize>, replacement: &str) -> Result<String, Refusal> {
        if range.start > range.end {
            return Err(Refusal::NotProvable);
        }
        let head = self.text.get(..range.start).ok_or(Refusal::NotProvable)?;
        let tail = self.text.get(range.end..).ok_or(Refusal::NotProvable)?;
        Ok(format!("{head}{replacement}{tail}"))
    }

    // ---- what a path names ----

    /// The plain key at `path`, `None` when the file has nothing there (a new key can be made, or there is nothing to clear), or a
    /// refusal when the path runs into a construct the patcher does not edit. Clearing a key under a plain value or under a table made of
    /// dotted keys finds nothing, because the leaves of such a table are all known; under an inline table or an array of tables it
    /// cannot tell and refuses.
    fn target(&self, path: &[String], mode: Mode) -> Result<Option<&Leaf>, Refusal> {
        let n = path.len();
        for k in 1..=n {
            if let Some(s) = self
                .sections
                .iter()
                .find(|s| s.kind == Kind::ArrayItem && s.path == path[..k])
            {
                return Err(
                    self.unsupported(s.header.start, "the key is inside an array of tables")
                );
            }
        }
        if let Some(l) = self.leaves.iter().find(|l| l.path == path) {
            if l.shape == Shape::Inline {
                return Err(self.unsupported(l.lead, "the value is an inline table"));
            }
            if l.dotted {
                return Err(self.unsupported(l.lead, "the key is defined with dotted keys"));
            }
            return Ok(Some(l));
        }
        // The path names a table: a standard table, a table made of dotted keys, or a table that holds only arrays of tables.
        if let Some(s) = self
            .sections
            .iter()
            .find(|s| s.kind != Kind::Root && s.path.starts_with(path))
        {
            return Err(self.unsupported(s.header.start, "the key names a table"));
        }
        if let Some(l) = self
            .leaves
            .iter()
            .find(|l| l.dotted && l.path.len() > n && l.path.starts_with(path))
        {
            return Err(self.unsupported(l.lead, "the table is defined with dotted keys"));
        }
        // Nothing is at the path; what stands above it decides whether a key can be made (Set) or is simply not there (Clear).
        for k in 1..n {
            let prefix = &path[..k];
            if let Some(l) = self.leaves.iter().find(|l| l.path == prefix) {
                if mode == Mode::Clear && l.shape == Shape::Plain {
                    return Ok(None);
                }
                return Err(
                    self.unsupported(l.lead, "the parent key holds a value, not a standard table")
                );
            }
            if let Some(l) = self.leaves.iter().find(|l| {
                l.dotted
                    && l.path.len() > k
                    && l.path.starts_with(prefix)
                    && k > self.sections[l.section].path.len()
            }) {
                if mode == Mode::Clear {
                    return Ok(None);
                }
                return Err(self.unsupported(l.lead, "the table is defined with dotted keys"));
            }
        }
        Ok(None)
    }

    /// Whether the file already holds `value` at `path`.
    fn holds(&self, path: &[String], value: &EditValue) -> bool {
        let Ok(table) = toml::from_str::<toml::Table>(self.text) else {
            return false;
        };
        let Some((first, rest)) = path.split_first() else {
            return false;
        };
        let mut found = table.get(first);
        for segment in rest {
            found = found
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(segment));
        }
        found.is_some_and(|v| Atom::of_value(v) == Atom::of_edit(value))
    }

    // ---- set ----

    fn set(
        &self,
        path: &[String],
        value: &EditValue,
        rank: &dyn Fn(&[String]) -> usize,
        shift: isize,
    ) -> Result<Option<String>, Refusal> {
        if let Some(leaf) = self.target(path, Mode::Set)? {
            if self.holds(path, value) {
                return Ok(None);
            }
            let range = shifted(&leaf.value, shift)?;
            return self.splice(range, &render_value(value)).map(Some);
        }
        let Some((name, parent)) = path.split_last() else {
            return Err(Refusal::Unsupported {
                line: 0,
                reason: "the key path is empty",
            });
        };
        let line = format!(
            "{} = {}{}",
            render_key(name),
            render_value(value),
            self.eol()
        );
        if parent.is_empty() {
            return Ok(Some(self.insert_top(name, &line, rank)));
        }
        if let Some(section) = self
            .sections
            .iter()
            .position(|s| s.kind == Kind::Header && s.path == parent)
        {
            return Ok(Some(self.insert_in(section, &line)));
        }
        Ok(Some(self.append_table(parent, &line)))
    }

    /// A new top-level key.
    fn insert_top(&self, name: &str, line: &str, rank: &dyn Fn(&[String]) -> usize) -> String {
        let wanted = rank(&[name.to_string()]);
        let top: Vec<&Leaf> = self
            .leaves
            .iter()
            .filter(|l| self.sections[l.section].kind == Kind::Root)
            .collect();
        let follows = |l: &&Leaf| {
            l.path.first().is_some_and(|first| {
                let r = rank(std::slice::from_ref(first));
                r != usize::MAX && r > wanted
            })
        };
        if let Some(next) = top.iter().copied().find(follows) {
            let at = self.attached_start(self.line_start(next.lead));
            return self.insert_at(at, line);
        }
        if let Some(end) = top.iter().map(|l| l.value.end).max() {
            return self.insert_at(self.line_end(end), line);
        }
        let first_header = self
            .sections
            .iter()
            .filter(|s| s.kind != Kind::Root)
            .map(|s| s.header.start)
            .min();
        match first_header {
            Some(h) => {
                let at = self.attached_start(self.line_start(h));
                self.insert_at(at, &format!("{line}{}", self.eol()))
            }
            None => self.insert_at(self.text.len(), line),
        }
    }

    /// A new key in the standard table `sections[section]`: after its last key, or after its header line when it has none.
    fn insert_in(&self, section: usize, line: &str) -> String {
        let at = match self
            .leaves
            .iter()
            .filter(|l| l.section == section)
            .map(|l| l.value.end)
            .max()
        {
            Some(end) => self.line_end(end),
            None => self.line_end(self.sections[section].header.end),
        };
        self.insert_at(at, line)
    }

    /// A new table at the end of the file, after one blank line, with its first key.
    fn append_table(&self, table: &[String], line: &str) -> String {
        let eol = self.eol();
        let mut out = self.text.to_owned();
        if !out.is_empty() {
            // The last line, not counting the line break that ends the text: a blank line there already is the separator.
            let body = out.strip_suffix('\n').unwrap_or(&out);
            let last_line_blank = body
                .rsplit('\n')
                .next()
                .is_some_and(|l| l.trim().is_empty());
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            if !last_line_blank {
                out.push_str(eol);
            }
        }
        let header: Vec<String> = table.iter().map(|k| render_key(k)).collect();
        out.push('[');
        out.push_str(&header.join("."));
        out.push(']');
        out.push_str(eol);
        out.push_str(line);
        out
    }

    // ---- clear ----

    fn clear(&self, path: &[String]) -> Result<Option<String>, Refusal> {
        let Some(leaf) = self.target(path, Mode::Clear)? else {
            return Ok(None);
        };
        let start = self.line_start(leaf.lead);
        let end = self.line_end(leaf.value.end);
        let only_key = self
            .leaves
            .iter()
            .filter(|l| l.section == leaf.section)
            .count()
            == 1;
        if only_key
            && self.sections[leaf.section].kind == Kind::Header
            && let Some(range) = self.emptied_table(&self.sections[leaf.section], start, end)
        {
            return self.splice(range, "").map(Some);
        }
        self.splice(start..end, "").map(Some)
    }

    /// The lines to remove when taking the only key (the lines `leaf_start..leaf_end`) out of `section` leaves a table with no comment:
    /// its header, the blank lines between header and key, the key, and one blank line (the one before the header, else the first one
    /// after the table).
    fn emptied_table(
        &self,
        section: &Section,
        leaf_start: usize,
        leaf_end: usize,
    ) -> Option<Range<usize>> {
        let header_start = self.line_start(section.header.start);
        let header_end = self.line_end(section.header.end);
        if self.slice(section.header.end..header_end).contains('#')
            || self.attached_start(header_start) != header_start
            || !self.is_blank(header_end..leaf_start)
        {
            return None;
        }
        let body_end = self
            .sections
            .iter()
            .filter(|s| s.kind != Kind::Root && s.header.start > section.header.start)
            .map(|s| s.header.start)
            .min()
            .map_or(self.text.len(), |next| {
                self.attached_start(self.line_start(next))
            })
            .max(leaf_end);
        if !self.is_blank(leaf_end..body_end) {
            return None;
        }
        let mut from = header_start;
        let mut to = leaf_end;
        if header_start > 0 {
            let before = self.line_start(header_start - 1);
            if self.slice(before..header_start).trim().is_empty() {
                from = before;
            }
        }
        if from == header_start && leaf_end < body_end {
            let next_line = self.line_end(leaf_end);
            if self.slice(leaf_end..next_line).trim().is_empty() {
                to = next_line;
            }
        }
        Some(from..to)
    }
}

fn shifted(range: &Range<usize>, shift: isize) -> Result<Range<usize>, Refusal> {
    if shift == 0 {
        return Ok(range.clone());
    }
    let start = range
        .start
        .checked_add_signed(shift)
        .ok_or(Refusal::NotProvable)?;
    let end = range
        .end
        .checked_add_signed(shift)
        .ok_or(Refusal::NotProvable)?;
    Ok(start..end)
}

// ---- writing values ---------------------------------------------------------------------------------------------------------------

/// A key as TOML text: bare when it is made of letters, digits, `_` and `-`, else a basic quoted string.
fn render_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        key.to_owned()
    } else {
        quote(key)
    }
}

fn render_value(value: &EditValue) -> String {
    match value {
        EditValue::Bool(b) => b.to_string(),
        EditValue::Int(i) => i.to_string(),
        // `Debug` is the shortest text that reads back as the same number and always has a fraction or an exponent.
        EditValue::Float(f) => format!("{f:?}"),
        EditValue::Str(s) => quote(s),
        EditValue::StrList(items) => {
            let parts: Vec<String> = items.iter().map(|s| quote(s)).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

/// A basic double-quoted string. Every control character, the line and paragraph separators and the characters that make text read
/// in another order or hide it are written as `\uXXXX`, so the file never holds a raw character that a terminal or a reader could act on.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() || hides_text(c) => {
                // `\u` takes exactly four hexadecimal digits and `\U` exactly eight.
                let n = u32::from(c);
                if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04X}"));
                } else {
                    out.push_str(&format!("\\U{n:08X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Characters that are not control characters but change how text reads or hide it: the ones the terminal sanitizer marks
/// (`sanitize::marker`: bidirectional controls, zero-width and invisible characters, fillers, variation selectors, tag characters) and
/// the line and paragraph separators.
fn hides_text(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}') || crate::sanitize::marker(c).is_some()
}

// ---- the proof -------------------------------------------------------------------------------------------------------------------

/// A value in a form that compares: floats by their bits, so a NaN in the file equals itself.
#[derive(Debug, PartialEq, Eq)]
enum Atom {
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(String),
    Datetime(String),
    Array(Vec<Atom>),
    Table(BTreeMap<String, Atom>),
}

impl Atom {
    fn of_value(v: &toml::Value) -> Self {
        match v {
            toml::Value::Boolean(b) => Self::Bool(*b),
            toml::Value::Integer(i) => Self::Int(*i),
            toml::Value::Float(f) => Self::Float(f.to_bits()),
            toml::Value::String(s) => Self::Str(s.clone()),
            toml::Value::Datetime(d) => Self::Datetime(d.to_string()),
            toml::Value::Array(items) => Self::Array(items.iter().map(Self::of_value).collect()),
            toml::Value::Table(t) => Self::Table(
                t.iter()
                    .map(|(k, v)| (k.clone(), Self::of_value(v)))
                    .collect(),
            ),
        }
    }

    fn of_edit(v: &EditValue) -> Self {
        match v {
            EditValue::Bool(b) => Self::Bool(*b),
            EditValue::Int(i) => Self::Int(*i),
            EditValue::Float(f) => Self::Float(f.to_bits()),
            EditValue::Str(s) => Self::Str(s.clone()),
            EditValue::StrList(items) => {
                Self::Array(items.iter().map(|s| Self::Str(s.clone())).collect())
            }
        }
    }
}

/// A leaf of the tree, or a table with no entry.
#[derive(Debug, PartialEq, Eq)]
enum Flat {
    Leaf(Atom),
    EmptyTable,
}

fn flatten(table: &toml::Table, prefix: &mut Vec<String>, out: &mut BTreeMap<Vec<String>, Flat>) {
    for (key, value) in table {
        prefix.push(key.clone());
        match value {
            toml::Value::Table(inner) if inner.is_empty() => {
                out.insert(prefix.clone(), Flat::EmptyTable);
            }
            toml::Value::Table(inner) => flatten(inner, prefix, out),
            other => {
                out.insert(prefix.clone(), Flat::Leaf(Atom::of_value(other)));
            }
        }
        prefix.pop();
    }
}

/// Test hook: the proof on a given pair of texts and edit. Not part of the interface.
#[doc(hidden)]
pub fn prove_pair(old: &str, new: &str, edit: &Edit) -> Result<(), Refusal> {
    prove(old, new, edit)
}

fn flat_of(text: &str) -> Result<BTreeMap<Vec<String>, Flat>, Refusal> {
    let table = toml::from_str::<toml::Table>(text).map_err(|_| Refusal::NotProvable)?;
    let mut out = BTreeMap::new();
    flatten(&table, &mut Vec::new(), &mut out);
    Ok(out)
}

/// Requires that `new` reads as `old` with exactly the intended key changed. A table above the edited key may appear or vanish with it
/// (an emptied table goes, a new table comes), so an empty table there is left out of the comparison; a value there is not.
fn prove(old: &str, new: &str, edit: &Edit) -> Result<(), Refusal> {
    let mut want = flat_of(old)?;
    let mut got = flat_of(new)?;
    let path = match edit {
        Edit::Set { path, value } => {
            want.insert(path.clone(), Flat::Leaf(Atom::of_edit(value)));
            path
        }
        Edit::Clear { path } => {
            want.remove(path);
            path
        }
    };
    for k in 1..path.len() {
        for flat in [&mut want, &mut got] {
            if flat.get(&path[..k]) == Some(&Flat::EmptyTable) {
                flat.remove(&path[..k]);
            }
        }
    }
    if want == got {
        Ok(())
    } else {
        Err(Refusal::NotProvable)
    }
}
