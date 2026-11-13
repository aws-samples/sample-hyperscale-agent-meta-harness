//! The engine file on the client.
//!
//! A user can keep one TOML file per engine, `~/.config/ham/engines/<engine>.toml`. `ham` reads it on the Mac, turns it into typed values and
//! sends them with the run as the optional `engineConfig` member of the start request; comments never leave the Mac. This module is the whole
//! of that reading, and it names no engine: the engine is an argument, and the key table it is checked against is `harness-protocol`'s
//! (`keys_for`), the same one that control-api and agentd check again.
//!
//! - **One buffer.** [`EngineFile::load`] reads the file once through a [`Source`] (the disk, or a test's double that counts calls): a regular
//!   file, at most [`MAX_BYTES`] and one byte more are held, a link is followed, a directory or a device is refused. The verbs and the request all
//!   read from that buffer ([`EngineFile::read`]); the file is never opened a second time.
//! - **Checks, in order.** Size, UTF-8, NUL and a byte-order mark, the nesting of brackets, then the `toml` crate's own parse (with spans, so every
//!   problem has its line). `schema` leaves the table and must be `1` once any key is set; a newer number is named as such. The rest is converted
//!   to a JSON object (a boolean, a whole number, a finite number, a text, a list of text; anything else is `bad_type`) and handed to
//!   [`harness_protocol::direct::check_engine_config`], the one check. Its first problem gets the line of its key.
//! - **No echo.** A message names a key only when the protocol's rule allows it (short plain lowercase text) and never repeats a value or a line of
//!   the file, so a symlink to a credentials file prints nothing of it, and no escape sequence of a key reaches a terminal.
//! - **Nothing to send.** An absent file, an empty one, a file with only `schema = 1` or empty tables sends no member: [`Reading::config`] is
//!   `None` and a default user is byte for byte the user of protocol 1.38.0.

use crate::config::{config_path, nesting_past, position};
use crate::engines;
use crate::sanitize::sanitize_line;
use crate::settings::{MAX_CONFIG_BYTES, MAX_NESTING};
use harness_protocol::direct::{
    ENGINE_CONFIG_SCHEMA, EngineConfig, EngineConfigProblem, check_engine_config, keys_for,
};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// The largest engine file `ham` looks at: the same 64 KiB as `config.toml`.
pub const MAX_BYTES: usize = MAX_CONFIG_BYTES;

/// The deepest table nesting the conversion follows. The protocol allows three levels in all; a file that goes past this is `too_deep`.
const MAX_TABLE_DEPTH: usize = 8;

/// A problem with the message and the line, never a value and never a line of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// A stable word: the protocol's own (`unknown_key`, `denied_key`, `bad_type`, `out_of_range`, `duplicate_value`, `too_large`, `too_deep`,
    /// `too_many_keys`, `string_too_long`) or the reader's (`syntax`, `not_utf8`, `nul`, `bom`, `missing_schema`, `newer_schema`, `bad_schema`).
    pub code: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// The dotted name of the key, when it is short plain text.
    pub key: Option<String>,
    pub message: String,
}

impl Problem {
    /// `engines/<engine>.toml line 4: key "aws.profile" is not allowed: ...`, with the column after the line when the parser gave one.
    pub fn render(&self, shown: &str) -> String {
        let at = match (self.line, self.column) {
            (Some(l), Some(c)) => format!(" line {l} column {c}"),
            (Some(l), None) => format!(" line {l}"),
            _ => String::new(),
        };
        sanitize_line(&format!("{shown}{at}: {}", self.message))
    }
}

/// A key the file sets, for `ham engine <name> show` and the Settings screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetKey {
    /// The dotted path in the key table (`reasoning.effort`).
    pub path: &'static str,
    /// The value as the card shows it: a text as it is, a list as its members joined by commas, a number as written.
    pub value: String,
    pub line: Option<usize>,
}

/// What the file says once it is read: the member to send, and the problems that refuse it.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    /// The settings to send. `None` when the file sets no key, and when it has a problem.
    pub config: Option<EngineConfig>,
    pub problems: Vec<Problem>,
    /// The line of every key of the file, as a dotted path, in file order.
    lines: Vec<KeyLine>,
    /// The engine the file was read for.
    engine: String,
    /// What the file holds, converted, even when the check refused it: the Settings screen shows the value of a key beside the finding.
    /// Empty when the file could not be converted at all.
    values: Map<String, Value>,
}

/// One key of the file: its dotted path, its line, and whether its name is plain enough for a message to repeat.
#[derive(Clone, Debug)]
struct KeyLine {
    path: String,
    line: usize,
    plain: bool,
}

impl Reading {
    /// True when the file has no problem.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// The line of the key `path` (a dotted path of the file), when the file has it.
    pub fn line_of(&self, path: &str) -> Option<usize> {
        self.lines.iter().find(|k| k.path == path).map(|k| k.line)
    }

    /// The value the file holds for the key `path` of the engine's table (a dotted path), even when the file as a whole is refused.
    pub fn value(&self, path: &str) -> Option<&Value> {
        let mut parts = path.split('.');
        let mut cur = self.values.get(parts.next()?)?;
        for part in parts {
            cur = cur.as_object()?.get(part)?;
        }
        Some(cur)
    }

    /// The keys of the engine's table that the file sets, in the table's order.
    pub fn set_keys(&self) -> Vec<SetKey> {
        keys_for(&self.engine)
            .unwrap_or(&[])
            .iter()
            .filter_map(|row| {
                self.value(row.path).map(|v| SetKey {
                    path: row.path,
                    value: value_text(v),
                    line: self.line_of(row.path),
                })
            })
            .collect()
    }

    /// One line per problem, `shown` being the name of the file in a message (`engines/<engine>.toml`).
    pub fn render(&self, shown: &str) -> Vec<String> {
        self.problems.iter().map(|p| p.render(shown)).collect()
    }

    /// The text of a refusal at the start of a run: the problems, then that nothing was sent.
    pub fn refusal(&self, shown: &str) -> String {
        let lines = self.render(shown);
        match lines.as_slice() {
            [one] => format!("{one}. Nothing was sent."),
            many => format!("{}\nNothing was sent.", many.join("\n")),
        }
    }
}

/// A value as the card and `show` word it: a text as it is, a list as its members joined by commas, a number as it is written.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

// ---- where the bytes come from -----------------------------------------------------------------------------------------------------

/// The engine file of `engine`: `<config dir>/ham/engines/<engine>.toml`, beside `config.toml`. Only an engine of the roadmap has a file, so the name
/// is never a path.
pub fn engine_path(env: &dyn Fn(&str) -> Option<String>, engine: &str) -> Option<PathBuf> {
    engines::find(engine)?;
    let config = config_path(env)?;
    Some(
        config
            .parent()?
            .join("engines")
            .join(format!("{engine}.toml")),
    )
}

/// The name of the engine file in a message: `engines/<engine>.toml`.
pub fn shown_path(engine: &str) -> String {
    format!("engines/{engine}.toml")
}

/// Where the bytes of a file come from. The product reads the disk ([`Disk`]); a test passes a double that counts the reads.
pub trait Source {
    /// The bytes of the file at `path` (at most [`MAX_BYTES`] and one more) and its size on disk; `None` when there is no file. A link is followed;
    /// a directory, a device or a pipe is an error.
    fn read(&self, path: &Path) -> Result<Option<(Vec<u8>, u64)>, String>;
}

/// The disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct Disk;

impl Source for Disk {
    fn read(&self, path: &Path) -> Result<Option<(Vec<u8>, u64)>, String> {
        let shown = path.display();
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{shown}: {e}")),
        };
        if !meta.is_file() {
            return Err(format!("{shown}: not a regular file"));
        }
        let file = std::fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
        // The handle is checked again: what was opened is what was looked at.
        let opened = file.metadata().map_err(|e| format!("{shown}: {e}"))?;
        if !opened.is_file() {
            return Err(format!("{shown}: not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{shown}: {e}"))?;
        Ok(Some((bytes, opened.len())))
    }
}

/// The engine file as it is on disk: its bytes (at most the cap and one more), its size, and the SHA-256 of those bytes. Read once.
#[derive(Clone, Debug)]
pub struct EngineFile {
    path: PathBuf,
    bytes: Vec<u8>,
    file_len: u64,
    exists: bool,
    digest: Option<[u8; 32]>,
}

impl EngineFile {
    /// Reads the file at `path` through `source`, once. An error is a file that cannot be read at all (not a regular file, an I/O error).
    pub fn load(path: &Path, source: &dyn Source) -> Result<EngineFile, String> {
        Ok(match source.read(path)? {
            None => EngineFile {
                path: path.to_path_buf(),
                bytes: Vec::new(),
                file_len: 0,
                exists: false,
                digest: None,
            },
            Some((bytes, file_len)) => EngineFile {
                path: path.to_path_buf(),
                digest: Some(Sha256::digest(&bytes).into()),
                bytes,
                file_len,
                exists: true,
            },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when there is a file.
    pub fn exists(&self) -> bool {
        self.exists
    }

    /// The size of the file on disk.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// The bytes held: never more than the cap and one more.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The text of the file; empty for a file that is over the cap or is not UTF-8 (read the problems through [`EngineFile::read`] first).
    pub fn text(&self) -> &str {
        if self.bytes.len() > MAX_BYTES {
            return "";
        }
        std::str::from_utf8(&self.bytes).unwrap_or("")
    }

    /// The SHA-256 of the bytes held, as 64 lowercase hex digits; `None` when there is no file.
    pub fn file_sha256(&self) -> Option<String> {
        self.digest
            .map(|d| d.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The digest the writer compares just before it renames a new file over this one.
    pub fn digest(&self) -> Option<[u8; 32]> {
        self.digest
    }

    /// What the file says for `engine`.
    pub fn read(&self, engine: &str) -> Reading {
        read_bytes(&self.bytes, self.file_len, engine)
    }
}

// ---- reading -----------------------------------------------------------------------------------------------------------------------

fn problem(code: &str, line: Option<usize>, column: Option<usize>, message: String) -> Problem {
    Problem {
        code: code.to_string(),
        line,
        column,
        key: None,
        message,
    }
}

fn reading_of(engine: &str, p: Problem) -> Reading {
    Reading {
        config: None,
        problems: vec![p],
        lines: Vec::new(),
        engine: engine.to_string(),
        values: Map::new(),
    }
}

/// What the file holds as a message words it: the TOML type, never the value.
fn found(v: &DeValue<'_>) -> &'static str {
    match v {
        DeValue::String(_) => "a text",
        DeValue::Integer(_) => "a whole number",
        DeValue::Float(_) => "a number",
        DeValue::Boolean(_) => "true or false",
        DeValue::Datetime(_) => "a date or time",
        DeValue::Array(_) => "a list",
        DeValue::Table(_) => "a table",
    }
}

/// The dotted name of a path when it is short plain text (`[a-z0-9_-]` parts joined by dots, at most 40 bytes), else `None`: the rule of the
/// protocol, so a message never repeats an escape sequence or a control character.
fn safe_key(path: &[String]) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let plain = |s: &String| {
        !s.is_empty()
            && s.bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    };
    if !path.iter().all(plain) {
        return None;
    }
    let joined = path.join(".");
    (joined.len() <= 40).then_some(joined)
}

fn subject(key: &Option<String>) -> String {
    match key {
        Some(k) => format!("key \"{k}\""),
        None => "a key with unusual characters".to_string(),
    }
}

/// The name of a key of the parsed document.
fn name_of<'a>(k: &'a Spanned<std::borrow::Cow<'_, str>>) -> &'a str {
    k.get_ref()
}

/// A whole number of a TOML integer, when it fits an `i64`.
fn whole(i: &toml::de::DeInteger<'_>) -> Option<i64> {
    i64::from_str_radix(i.as_str(), i.radix()).ok()
}

/// The conversion of the parsed file to JSON, with the line of each key and the problems found on the way.
struct Convert<'t> {
    text: &'t str,
    problems: Vec<Problem>,
    lines: Vec<KeyLine>,
}

impl Convert<'_> {
    fn line(&self, key: &Spanned<std::borrow::Cow<'_, str>>) -> usize {
        position(self.text, key.span().start).0
    }

    fn bad(&mut self, code: &str, path: &[String], line: usize, what: String) {
        let key = safe_key(path);
        let message = format!("{} {what}", subject(&key));
        self.problems.push(Problem {
            code: code.to_string(),
            line: Some(line),
            column: None,
            key,
            message,
        });
    }

    fn table(
        &mut self,
        table: &DeTable<'_>,
        path: &mut Vec<String>,
        skip: &[&str],
    ) -> Map<String, Value> {
        let mut entries: Vec<_> = table.iter().collect();
        entries.sort_by_key(|(k, _)| k.span().start);
        let mut out = Map::new();
        for (k, v) in entries {
            let name = name_of(k).to_string();
            if path.is_empty() && skip.contains(&name.as_str()) {
                continue;
            }
            let line = self.line(k);
            path.push(name.clone());
            self.lines.push(KeyLine {
                path: path.join("."),
                line,
                plain: safe_key(path).is_some(),
            });
            if let Some(value) = self.value(v.get_ref(), path, line) {
                out.insert(name, value);
            }
            path.pop();
        }
        out
    }

    fn value(&mut self, v: &DeValue<'_>, path: &mut Vec<String>, line: usize) -> Option<Value> {
        match v {
            DeValue::String(s) => Some(Value::String(s.to_string())),
            DeValue::Boolean(b) => Some(Value::Bool(*b)),
            DeValue::Integer(i) => match whole(i) {
                Some(n) => Some(Value::Number(Number::from(n))),
                None => {
                    self.bad(
                        "bad_type",
                        path,
                        line,
                        "holds a whole number that is too large".to_string(),
                    );
                    None
                }
            },
            DeValue::Float(f) => match f.as_str().parse::<f64>().ok().and_then(Number::from_f64) {
                Some(n) => Some(Value::Number(n)),
                None => {
                    self.bad(
                        "bad_type",
                        path,
                        line,
                        "holds a number that is not finite".to_string(),
                    );
                    None
                }
            },
            DeValue::Datetime(_) => {
                self.bad(
                    "bad_type",
                    path,
                    line,
                    "holds a date or time; a setting is a text, a number, true or false, or a list of text".to_string(),
                );
                None
            }
            DeValue::Array(items) => {
                let mut list = Vec::new();
                let mut ok = true;
                for item in items.iter() {
                    match item.get_ref() {
                        DeValue::String(s) => list.push(Value::String(s.to_string())),
                        other => {
                            ok = false;
                            let what = format!(
                                "holds a list with {} in it; a list holds text only",
                                found(other)
                            );
                            self.bad("bad_type", path, line, what);
                            break;
                        }
                    }
                }
                ok.then_some(Value::Array(list))
            }
            DeValue::Table(t) => {
                if path.len() > MAX_TABLE_DEPTH {
                    self.bad(
                        "too_deep",
                        path,
                        line,
                        "nests tables deeper than any setting does".to_string(),
                    );
                    return None;
                }
                Some(Value::Object(self.table(t, path, &[])))
            }
        }
    }
}

/// What the bytes of an engine file say for `engine`. `file_len` is the size on disk (it can be larger than `bytes`, which holds at most the cap and
/// one more). Never fails: the problems are in the reading.
pub fn read_bytes(bytes: &[u8], file_len: u64, engine: &str) -> Reading {
    let size = (bytes.len() as u64).max(file_len);
    if bytes.len() > MAX_BYTES || size > MAX_BYTES as u64 {
        return reading_of(
            engine,
            problem(
                "too_large",
                None,
                None,
                format!("the file is {size} bytes; the most ham reads is {MAX_BYTES}"),
            ),
        );
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => read_text(text, engine),
        Err(e) => {
            // The line of the first byte that is not UTF-8; the bytes themselves are never repeated.
            let line = bytes[..e.valid_up_to()]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1;
            reading_of(
                engine,
                problem(
                    "not_utf8",
                    Some(line),
                    None,
                    "the file is not UTF-8 text".to_string(),
                ),
            )
        }
    }
}

/// What the text of an engine file says for `engine`.
pub fn read_text(text: &str, engine: &str) -> Reading {
    if text.len() > MAX_BYTES {
        return reading_of(
            engine,
            problem(
                "too_large",
                None,
                None,
                format!(
                    "the file is {} bytes; the most ham reads is {MAX_BYTES}",
                    text.len()
                ),
            ),
        );
    }
    if let Some(at) = text.find('\0') {
        let (line, column) = position(text, at);
        return reading_of(
            engine,
            problem(
                "nul",
                Some(line),
                Some(column),
                "the file holds a NUL byte; it is not a text file".to_string(),
            ),
        );
    }
    if text.starts_with('\u{feff}') {
        return reading_of(
            engine,
            problem(
                "bom",
                Some(1),
                Some(1),
                "the file starts with a byte-order mark; save it as plain UTF-8 without one"
                    .to_string(),
            ),
        );
    }
    if let Some(at) = nesting_past(text, MAX_NESTING) {
        let (line, column) = position(text, at);
        return reading_of(
            engine,
            problem(
                "too_deep",
                Some(line),
                Some(column),
                format!(
                    "lists and tables nest more than {MAX_NESTING} deep; no setting needs that"
                ),
            ),
        );
    }
    let doc = match DeTable::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            let at = e.span().map_or(0, |s| s.start);
            let (line, column) = position(text, at);
            let message: String = sanitize_line(e.message()).chars().take(160).collect();
            return reading_of(engine, problem("syntax", Some(line), Some(column), message));
        }
    };
    let root = doc.get_ref();
    let mut convert = Convert {
        text,
        problems: Vec::new(),
        lines: Vec::new(),
    };

    // The schema line first: the meaning of every key depends on it.
    let mut entries: Vec<_> = root.iter().collect();
    entries.sort_by_key(|(k, _)| k.span().start);
    let first_line = entries.first().map(|(k, _)| convert.line(k));
    let schema = entries.iter().find(|(k, _)| name_of(k) == "schema");
    let others = entries.iter().any(|(k, _)| name_of(k) != "schema");
    match schema {
        None if others => {
            return reading_of(
                engine,
                problem(
                    "missing_schema",
                    first_line,
                    None,
                    "the file sets settings but has no `schema = 1` line; add it as the first line"
                        .to_string(),
                ),
            );
        }
        None => {}
        Some((k, v)) => {
            let line = Some(convert.line(k));
            match v.get_ref() {
                DeValue::Integer(i) => match whole(i) {
                    Some(n) if u64::try_from(n) == Ok(ENGINE_CONFIG_SCHEMA) => {}
                    Some(n) if n > 0 => {
                        return reading_of(
                            engine,
                            problem(
                                "newer_schema",
                                line,
                                None,
                                format!(
                                    "the file is written for schema {n}, which a newer ham reads; this ham reads schema {ENGINE_CONFIG_SCHEMA}: update ham, or change the file"
                                ),
                            ),
                        );
                    }
                    _ => {
                        return reading_of(
                            engine,
                            problem(
                                "bad_schema",
                                line,
                                None,
                                format!("schema must be {ENGINE_CONFIG_SCHEMA}"),
                            ),
                        );
                    }
                },
                other => {
                    let message = format!("`schema` takes a whole number, not {}", found(other));
                    return reading_of(engine, problem("bad_type", line, None, message));
                }
            }
        }
    }

    let settings = convert.table(root, &mut Vec::new(), &["schema"]);
    if !convert.problems.is_empty() {
        let mut problems = convert.problems;
        problems.sort_by_key(|p| p.line);
        return Reading {
            config: None,
            problems,
            lines: convert.lines,
            engine: engine.to_string(),
            values: Map::new(),
        };
    }
    let values = settings.clone();
    let cfg = EngineConfig {
        engine: engine.to_string(),
        schema: ENGINE_CONFIG_SCHEMA,
        settings,
    };
    let lines = convert.lines;
    match check_engine_config(&cfg) {
        Ok(()) => Reading {
            config: (cfg.key_count() > 0).then_some(cfg),
            problems: Vec::new(),
            lines,
            engine: engine.to_string(),
            values,
        },
        Err(p) => Reading {
            config: None,
            problems: vec![from_check(&p, &lines)],
            lines,
            engine: engine.to_string(),
            values,
        },
    }
}

/// The line of a key the protocol named: the key itself, else the nearest table above it; for a key it would not name, the first key of the file
/// whose name has a character the messages do not repeat.
fn line_for(p: &EngineConfigProblem, lines: &[KeyLine]) -> Option<usize> {
    let find = |path: &str| lines.iter().find(|k| k.path == path).map(|k| k.line);
    match p.key() {
        Some(key) => {
            let mut path = key;
            loop {
                if let Some(l) = find(path) {
                    return Some(l);
                }
                path = path.rsplit_once('.')?.0;
            }
        }
        None => lines.iter().find(|k| !k.plain).map(|k| k.line),
    }
}

/// The words of one problem of the shared check: the protocol's own, without their `engine config:` lead-in, with the invariant a denied key
/// protects after the reason. Never a value.
pub fn describe(p: &EngineConfigProblem) -> String {
    let text = p.to_string();
    let mut message = text
        .strip_prefix("engine config: ")
        .unwrap_or(&text)
        .to_string();
    if let Some(class) = p.denied_class() {
        message.push_str(&format!(" ({})", class.protects));
    }
    message
}

/// The reader's problem for one problem of the shared check, with the line of the key.
fn from_check(p: &EngineConfigProblem, lines: &[KeyLine]) -> Problem {
    Problem {
        code: p.code().to_string(),
        line: line_for(p, lines),
        column: None,
        key: p.key().map(str::to_string),
        message: describe(p),
    }
}
