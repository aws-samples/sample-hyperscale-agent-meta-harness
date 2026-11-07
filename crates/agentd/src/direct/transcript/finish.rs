//! From a raw record payload to a stored one: credential-file output withheld, binary output replaced
//! by its size, a huge payload cut before the redaction rules run, every string and property name redacted, and the payload cut to the
//! record cap with its head and its tail, a digest of what was cut and a mark. The order is fixed: never redact a cut payload, never cut
//! before the pre-cap, never store a payload the rules could not run on.

use crate::direct::transcript::settings::Settings;
use harness_core::redact::{RedactFailed, Redactor, redact_checked};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// What replaces the output of a tool that read a credential file.
pub const WITHHELD: &str = "[file contents withheld]";
/// The marker member of a payload that was cut.
pub const TRUNCATED_KEY: &str = "_truncated";
/// The room the `_truncated` member takes inside the record cap.
const MARK_ROOM: usize = 160;
/// The room the cut marker takes inside a cut string.
const CUT_MARKER_ROOM: usize = 64;
/// A string shorter than this is never the thing to cut.
const MIN_CUT: usize = 256;
/// A string of at least this many characters is tested for binary content.
const BINARY_MIN_CHARS: usize = 64;

/// A payload ready to store, and what was done to it.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub data: Value,
    /// The payload was cut to the record cap or replaced by its digest.
    pub truncated: bool,
    /// Redaction could not run and the payload was replaced.
    pub suppressed: bool,
    /// Binary output or a credential file's output was left out.
    pub excluded: bool,
}

fn ser_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- credential files -----------------------------------------------------------------------------------------------------------

/// Whether a file name is one whose contents are credentials: `.env`, `.env.*`, `.envrc`, `*.pem`, `id_*` (a key, not a source file),
/// `.npmrc`, `.netrc` and `credentials` (also with a data extension).
pub fn is_credential_file(name: &str) -> bool {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    if base.is_empty() {
        return false;
    }
    if base == ".env"
        || base.starts_with(".env.")
        || base == ".envrc"
        || base == ".npmrc"
        || base == ".netrc"
        || base == "_netrc"
    {
        return true;
    }
    if base.len() > 4 && base.ends_with(".pem") {
        return true;
    }
    if let Some(rest) = base.strip_prefix("id_") {
        let stem = rest.strip_suffix(".pub").unwrap_or(rest);
        return !stem.is_empty() && !stem.contains('.');
    }
    if let Some(rest) = base.strip_prefix("credentials") {
        return rest.is_empty()
            || matches!(
                rest,
                ".json" | ".yaml" | ".yml" | ".toml" | ".csv" | ".ini" | ".xml"
            );
    }
    false
}

fn tokens_name_a_credential_file(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '=' | ',' | ';' | '(' | ')' | '<' | '>' | '|' | '&' | ':'
            )
    })
    .filter(|t| !t.is_empty())
    .any(is_credential_file)
}

/// Whether a `tool_call` or `tool_call_update` names a credential file: in its title, a location, or a path or command of its input.
pub fn names_credential_file(update: &Value) -> bool {
    if update
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(tokens_name_a_credential_file)
    {
        return true;
    }
    if update
        .get("locations")
        .and_then(Value::as_array)
        .is_some_and(|l| {
            l.iter()
                .filter_map(|x| x.get("path").and_then(Value::as_str))
                .any(tokens_name_a_credential_file)
        })
    {
        return true;
    }
    let mut stack: Vec<&Value> = update.get("rawInput").into_iter().collect();
    let mut seen = 0usize;
    while let Some(v) = stack.pop() {
        seen += 1;
        if seen > 256 {
            break;
        }
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if let Value::String(s) = x
                        && matches!(
                            k.as_str(),
                            "path"
                                | "file"
                                | "file_path"
                                | "filePath"
                                | "filepath"
                                | "command"
                                | "cmd"
                                | "filename"
                                | "target"
                        )
                        && tokens_name_a_credential_file(&s.chars().take(2048).collect::<String>())
                    {
                        return true;
                    }
                    if x.is_object() || x.is_array() {
                        stack.push(x);
                    }
                }
            }
            Value::Array(a) => stack.extend(a.iter()),
            Value::String(s)
                if tokens_name_a_credential_file(&s.chars().take(2048).collect::<String>()) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Replaces the output members of a tool payload by [`WITHHELD`].
fn withhold(data: &mut Value) -> bool {
    let mut did = false;
    if let Some(m) = data.as_object_mut() {
        for key in ["rawOutput", "content"] {
            if m.contains_key(key) {
                m.insert(key.to_string(), Value::String(WITHHELD.to_string()));
                did = true;
            }
        }
    }
    did
}

// ---- binary output --------------------------------------------------------------------------------------------------------------

fn looks_binary(s: &str) -> bool {
    let (mut total, mut odd) = (0usize, 0usize);
    for c in s.chars().take(65_536) {
        total += 1;
        if (c < ' ' && !matches!(c, '\n' | '\r' | '\t')) || c == '\u{fffd}' {
            odd += 1;
        }
    }
    total >= BINARY_MIN_CHARS && odd * 10 > total
}

fn replace_binary(v: &mut Value, found: &mut bool) {
    match v {
        Value::String(s) if looks_binary(s) => {
            *v = Value::String(format!("[binary output withheld: {} bytes]", s.len()));
            *found = true;
        }
        Value::Array(a) => a.iter_mut().for_each(|x| replace_binary(x, found)),
        Value::Object(m) => m.values_mut().for_each(|x| replace_binary(x, found)),
        _ => {}
    }
}

fn binary_to_size(data: &mut Value) -> bool {
    let mut found = false;
    if let Some(m) = data.as_object_mut() {
        for key in ["rawOutput", "content"] {
            if let Some(x) = m.get_mut(key) {
                replace_binary(x, &mut found);
            }
        }
    }
    found
}

// ---- cutting ---------------------------------------------------------------------------------------------------------------------

/// `s` cut to about `keep` bytes: two thirds from its head, one third from its tail, a marker between.
fn cut_middle(s: &str, keep: usize) -> String {
    let keep = keep.min(s.len());
    let head_len = keep * 2 / 3;
    let tail_len = keep - head_len;
    let mut h = head_len;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail_len;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    if t <= h {
        return s.to_string();
    }
    format!("{}\n…[cut {} bytes]…\n{}", &s[..h], t - h, &s[t..])
}

/// A path to the longest string of `v`: the keys and indexes to follow.
fn longest_string(v: &Value) -> Option<(Vec<Step>, usize)> {
    let mut best: Option<(Vec<Step>, usize)> = None;
    let mut stack: Vec<(&Value, Vec<Step>)> = vec![(v, Vec::new())];
    while let Some((x, path)) = stack.pop() {
        match x {
            Value::String(s) => {
                if best.as_ref().is_none_or(|(_, n)| s.len() > *n) {
                    best = Some((path, s.len()));
                }
            }
            Value::Array(a) => {
                for (i, y) in a.iter().enumerate() {
                    let mut p = path.clone();
                    p.push(Step::Index(i));
                    stack.push((y, p));
                }
            }
            Value::Object(m) => {
                for (k, y) in m {
                    let mut p = path.clone();
                    p.push(Step::Key(k.clone()));
                    stack.push((y, p));
                }
            }
            _ => {}
        }
    }
    best
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

fn at_path<'a>(v: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    let mut cur = v;
    for s in path {
        cur = match (s, cur) {
            (Step::Key(k), Value::Object(m)) => m.get_mut(k)?,
            (Step::Index(i), Value::Array(a)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// `v` with its longest strings cut until it serializes to at most `budget` bytes; `None` when only structure is left to cut.
fn cut_to_fit(mut v: Value, budget: usize) -> Option<Value> {
    for _ in 0..64 {
        let size = ser_len(&v);
        if size <= budget {
            return Some(v);
        }
        let (path, len) = longest_string(&v)?;
        if len < MIN_CUT {
            return None;
        }
        let excess = size - budget;
        let keep = len.saturating_sub(excess + CUT_MARKER_ROOM).max(128);
        if keep >= len {
            return None;
        }
        let Some(Value::String(s)) = at_path(&mut v, &path) else {
            return None;
        };
        *s = cut_middle(s, keep);
    }
    None
}

// ---- redaction --------------------------------------------------------------------------------------------------------------------

/// `v` with every string and property name redacted; an error when the rules could not run.
pub fn redact_value(r: &Redactor, v: &Value) -> Result<Value, RedactFailed> {
    Ok(match v {
        Value::String(s) => Value::String(redact_checked(r, s)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| redact_value(r, x))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                out.insert(redact_checked(r, k)?, redact_value(r, x)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// A payload that could not be kept as it is.
fn replaced(reason: &str, original_bytes: usize, digest: &str) -> Value {
    json!({ TRUNCATED_KEY: {"originalBytes": original_bytes, "sha256": digest, "reason": reason} })
}

/// The deepest nesting the reader accepts in one record's payload (`ham_core::transcript::MAX_RECORD_DEPTH`, the root being depth 0 and a
/// scalar at depth 32 allowed). The reader refuses the WHOLE chunk past it, and a stored object can never be rewritten.
pub const MAX_DEPTH: usize = 32;

/// Replaces every non-empty array or object at depth [`MAX_DEPTH`] by a short text, so nothing below that depth reaches a record. Returns
/// whether anything was cut. The engine's JSON is at most 128 levels deep (the parser's limit), so the recursion is bounded.
fn limit_depth(v: &mut Value, depth: usize) -> bool {
    let nonempty = match v {
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        _ => return false,
    };
    if depth >= MAX_DEPTH {
        if nonempty {
            let bytes = ser_len(v);
            *v = Value::String(format!("[nested too deeply: {bytes} bytes left out]"));
            return true;
        }
        return false;
    }
    let mut cut = false;
    match v {
        Value::Array(a) => {
            for i in a.iter_mut() {
                cut |= limit_depth(i, depth + 1);
            }
        }
        Value::Object(m) => {
            for i in m.values_mut() {
                cut |= limit_depth(i, depth + 1);
            }
        }
        _ => {}
    }
    cut
}

/// The payload of a record on its way to storage. `sensitive` says the record belongs to a tool that read a credential file;
/// `already_redacted` says its text went through the stream window already.
pub fn prepare(
    r: &Redactor,
    s: &Settings,
    mut data: Value,
    sensitive: bool,
    already_redacted: bool,
) -> Prepared {
    let mut excluded = false;
    if sensitive {
        excluded |= withhold(&mut data);
    }
    excluded |= binary_to_size(&mut data);
    let depth_cut = limit_depth(&mut data, 0);
    // A payload far over the cap is cut first, so the rules never scan megabytes of one string.
    let raw_len = ser_len(&data);
    let mut pre_cut = false;
    if raw_len > s.pre_cap {
        pre_cut = true;
        data = cut_to_fit(data, s.pre_cap)
            .unwrap_or_else(|| replaced("record too structured to keep", raw_len, ""));
    }
    if !already_redacted {
        match redact_value(r, &data) {
            Ok(v) => data = v,
            Err(_) => {
                return Prepared {
                    data: json!({"suppressed": "redaction failed"}),
                    truncated: depth_cut,
                    suppressed: true,
                    excluded,
                };
            }
        }
    }
    let bytes = serde_json::to_vec(&data).unwrap_or_default();
    if bytes.len() <= s.record_cap {
        return Prepared {
            data,
            truncated: depth_cut,
            suppressed: false,
            excluded,
        };
    }
    let original = if pre_cut { raw_len } else { bytes.len() };
    let digest = sha256_hex(&bytes);
    let mark = json!({"originalBytes": original, "sha256": digest});
    let cap = s.record_cap;
    let cut = cut_to_fit(data, cap.saturating_sub(MARK_ROOM));
    let mut cut = cut.unwrap_or_else(|| json!({}));
    if cut.as_object().is_some_and(|m| m.is_empty()) {
        cut = replaced("record too structured to keep", original, &digest);
    } else if let Some(m) = cut.as_object_mut() {
        m.insert(TRUNCATED_KEY.to_string(), mark);
    }
    Prepared {
        data: cut,
        truncated: true,
        suppressed: false,
        excluded,
    }
}
