//! The result contract of the tool server.
//!
//! The engine's client turns the whole MCP result object into JSON text and cuts that text in the middle at 8,000 characters (JavaScript string
//! length, UTF-16 code units). So a result is `content` text only (never `structuredContent`, which would be sent twice), its SERIALIZED form
//! stays within the profile's cap (7,500 by default), and it says when the text was shortened. The cut is made on the text, never on the JSON:
//! each character is charged what it costs once escaped, so a cut cannot split an escape or leave the document invalid.
//!
//! A failure starts with `error: <word>[: detail]`, because an engine shows a result with `isError` to the model as a completed call whose text
//! says it failed; a JSON-RPC error is reserved for faults of the protocol. The detail is one short plain line.

use crate::direct::tools::backend::{FailureWord, ToolOutput};
use serde_json::{Value, json};

/// The serialized result is at most this many UTF-16 units unless the engine's profile says another number (`maxResultChars`).
pub const DEFAULT_RESULT_CHARS: usize = 7_500;
/// A cap below this is raised to it: a profile value that leaves no room for the notice would make every answer a notice.
pub const MIN_RESULT_CHARS: usize = 256;
/// A failure's detail is cut at this many characters.
pub const DETAIL_MAX_CHARS: usize = 120;

/// A cut is made at the last line end of what fits when that keeps at least this share (4 of 5) of it.
const LINE_CUT_KEEPS: (usize, usize) = (4, 5);

/// A result ready to send, and what the server counts about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    pub value: Value,
    /// The text was shortened by the backend or by the cap.
    pub truncated: bool,
    /// The bytes of the text, notice included.
    pub text_bytes: usize,
    /// The failure word, for a failed call.
    pub failure: Option<FailureWord>,
}

/// The length of `v` in UTF-16 code units of its compact JSON form: what `JSON.stringify(v).length` is in the engine's client.
pub fn serialized_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.encode_utf16().count())
}

/// The text of a failure: `error: <word>`, then `: <detail>` when the detail has anything in it after it is cleaned.
pub fn failure_text(word: FailureWord, detail: Option<&str>) -> String {
    match detail.map(clean_detail).filter(|d| !d.is_empty()) {
        Some(d) => format!("error: {}: {d}", word.as_str()),
        None => format!("error: {}", word.as_str()),
    }
}

/// One line: control characters and runs of white space become one space, the ends are trimmed, and at most [`DETAIL_MAX_CHARS`] characters stay.
fn clean_detail(s: &str) -> String {
    let words: Vec<&str> = s
        .split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    let line: String = words.join(" ").chars().take(DETAIL_MAX_CHARS).collect();
    line.trim_end().to_string()
}

fn wrap(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// What `c` costs in the serialized form of a JSON string, in UTF-16 units (serde_json's escapes: `\"`, `\\`, `\b \f \n \r \t`, and `\u00XX`
/// for the other control characters; everything else is written as it is).
fn escaped_units(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf16(),
    }
}

fn escaped_len(s: &str) -> usize {
    s.chars().map(escaped_units).sum()
}

fn notice(shown: usize, total: usize) -> String {
    format!("\n[shortened: showing {shown} of {total} characters]")
}

/// `text` cut to what fits `cap` once wrapped, with the notice; `None` when it fits whole.
fn fit(text: &str, cap: usize, is_error: bool) -> Option<String> {
    let overhead = serialized_len(&wrap("", is_error));
    if overhead + escaped_len(text) <= cap {
        return None;
    }
    let total = text.chars().count();
    // The notice is charged at its widest (the digits of the total twice), so that the digits of the real count cannot push it over.
    let budget = cap
        .saturating_sub(overhead)
        .saturating_sub(escaped_len(&notice(total, total)));
    let mut used = 0;
    let mut shown = 0;
    let mut end = 0;
    let mut last_line: Option<(usize, usize)> = None;
    for (at, c) in text.char_indices() {
        let units = escaped_units(c);
        if used + units > budget {
            break;
        }
        if c == '\n' {
            last_line = Some((shown, at));
        }
        used += units;
        shown += 1;
        end = at + c.len_utf8();
    }
    // The cut goes to the last line end when that keeps most of what fits: a match is not shown in half.
    if let Some((chars, at)) =
        last_line.filter(|(chars, _)| chars * LINE_CUT_KEEPS.1 >= shown * LINE_CUT_KEEPS.0)
    {
        shown = chars;
        end = at;
    }
    let out = format!("{}{}", &text[..end], notice(shown, total));
    // The budget is exact: `tests/direct_tools_result.rs` charges random text of every kind against it. The assertion turns a drift of the escape
    // table above (a character that serde_json writes longer than `escaped_units` says) into a failed test; the cap leaves 500 units under the
    // 8,000 at which the engine's client cuts, so a release build that ever drifted would not lose the end of an answer.
    debug_assert!(serialized_len(&wrap(&out, is_error)) <= cap);
    Some(out)
}

/// The result of a call: `output` as an MCP result whose serialized form is at most `cap` units (not below [`MIN_RESULT_CHARS`]).
pub fn render(output: &ToolOutput, cap: usize) -> Rendered {
    let cap = cap.max(MIN_RESULT_CHARS);
    let (text, by_backend, failure) = match output {
        ToolOutput::Text { text, truncated } => (text.clone(), *truncated, None),
        ToolOutput::Failed { word, detail } => {
            (failure_text(*word, detail.as_deref()), false, Some(*word))
        }
    };
    let is_error = failure.is_some();
    let (text, cut) = match fit(&text, cap, is_error) {
        Some(short) => (short, true),
        None => (text, false),
    };
    Rendered {
        value: wrap(&text, is_error),
        truncated: by_backend || cut,
        text_bytes: text.len(),
        failure,
    }
}
