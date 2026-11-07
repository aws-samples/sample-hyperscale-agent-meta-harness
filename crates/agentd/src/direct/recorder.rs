//! The recorder of a direct run: the evidence of how
//! the engine stopped, kept while it works and written at every stop. Until now the collector kept a title cut at about 100 characters and
//! a status per tool call and threw away the `rawInput` and `rawOutput` that the engine sends over ACP, so a command that exited non-zero
//! or hit the engine's 30 s shell cut read "completed" and nobody could tell afterwards why a run stopped. The engine's own `messages.json` is
//! engine-owned and unbounded and is not an input here.
//!
//! The recorder is bounded in memory for the whole run: a ring of the last [`RING_CALLS`] tool calls (each with the head of its input,
//! the tail of its output, its error text and the names of its input keys), the last [`MESSAGES_KEPT`] messages of the engine, and a few
//! counters. Everything it stores is redacted AT WRITE TIME (session secrets, key IDs, `name=value` secrets), never later. It adds
//! trusted anomaly lines the harness computed itself (a blank or unknown key in a tool input, an input that is not an object) because a
//! rule in Rust is a better witness than a model reading raw tool text. At each stop [`Recorder::stop_digest`] builds a digest of at
//! most [`STOP_DIGEST_MAX_BYTES`]; the driver writes it to S3 (key `direct_stop_digest_key` of harness-protocol) and the needs-input card and
//! the fresh-session digest read the same accessors. Nothing here decides anything about the work.
//!
//! The counters of the engine's own stops (identical calls in a row, failed calls in a row) are PM3's [`AttemptFacts`] in
//! `engine_stop.rs`, kept by the collector; the digest carries them as they are. [`RecorderCounters`] adds only what they do not have:
//! the run-wide call count, the run of calls with the same input (not only the same title) and the run of calls whose RECORDED result is
//! bad (a non-zero exit or a timeout the engine called "completed").

use crate::direct::classify::{clean_message, redact_key_ids, scrub_detail};
use crate::direct::engine_stop::AttemptFacts;
use crate::secrets::SecretSet;
use harness_protocol::direct::MAX_STOP_DIGEST_BYTES;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// Tool calls kept in the ring.
pub const RING_CALLS: usize = 40;
/// Longest title kept, in characters.
pub const TITLE_CHARS: usize = 120;
/// Head of a tool input kept, in bytes (cut at a character boundary).
pub const INPUT_HEAD_BYTES: usize = 8_192;
/// Tail of a tool output kept, in bytes (cut at a character boundary).
pub const OUTPUT_TAIL_BYTES: usize = 32_768;
/// Longest error text kept, in bytes.
pub const ERROR_TEXT_BYTES: usize = 4_096;
/// Input key names kept per call, and the longest name.
pub const KEYS_KEPT: usize = 12;
pub const KEY_NAME_CHARS: usize = 40;
/// Messages of the engine kept, and the longest one in bytes (the tail is kept).
pub const MESSAGES_KEPT: usize = 3;
pub const MESSAGE_BYTES: usize = 65_536;
/// Titles listed by [`Recorder::tool_notes`].
pub const NOTES_LISTED: usize = 30;
/// Anomaly lines kept (the newest).
pub const ANOMALIES_KEPT: usize = 16;
/// Longest tail of a failed test run kept, in bytes.
pub const TEST_TAIL_BYTES: usize = 65_536;
/// Longest `last_error`, in characters (the needs-input card's `evidence.lastError`).
pub const LAST_ERROR_CHARS: usize = 200;
/// The digest written at a stop never exceeds this many bytes.
pub const STOP_DIGEST_MAX_BYTES: usize = MAX_STOP_DIGEST_BYTES;
/// Fields of an older call that no longer fits whole are cut to this many bytes (compact form).
pub const COMPACT_FIELD_BYTES: usize = 160;
/// The engine cut every shell command at this many milliseconds up to fork series 14 (`Command timed out after 30000ms`). From fork patch 0015 the limit is
/// 600,000 ms by default and `CLINE_RUN_COMMANDS_TIMEOUT_MS` changes it; nothing here depends on the number, `timed_out` reads any digits.
pub const SHELL_CUT_MS: u64 = 30_000;

/// How the recorder reads the end of a call. The engine's own `status` is kept beside it; this is the harness's reading of the
/// output, so a command that exited 1 or was cut at 30 s is not "completed".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallVerdict {
    /// The call ended and nothing in its result says it failed.
    Ok,
    /// The engine said `failed`, the result carries an error or `success: false`, or the command exited non-zero.
    Failed,
    /// The result says the command timed out (`Command timed out after 30000ms`).
    TimedOut,
    /// No terminal update arrived (the turn ended first).
    NoResult,
}

impl CallVerdict {
    /// The word a [`ToolNote`] and the digest carry: `completed`, `failed`, `timed_out` or `no_result`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ok => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::NoResult => "no_result",
        }
    }
}

/// One line about one tool call: its title and how it ended (the recorder's reading, see [`CallVerdict::word`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolNote {
    /// At most [`TITLE_CHARS`] characters, redacted.
    pub title: String,
    pub status: String,
}

/// One tool call as kept in the ring. Every string is redacted and bounded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRecord {
    /// 1-based number of the call in the run.
    pub n: u64,
    /// 1-based attempt (one `session/prompt`) the call belongs to: [`Recorder::begin_prompt`] starts the next.
    pub attempt: u32,
    /// The engine's `toolCallId`, redacted and cut to 64 characters.
    pub id: String,
    /// The tool's name read from the title (`run_commands: ...`), empty when the title has none.
    pub tool: String,
    pub kind: String,
    pub title: String,
    /// The last `status` the engine sent for the call.
    pub engine_status: String,
    pub verdict: CallVerdict,
    /// At most [`INPUT_HEAD_BYTES`] of the input as compact JSON, and its full size before the cut.
    pub input_head: String,
    pub input_bytes: usize,
    pub input_keys: Vec<String>,
    /// At most [`OUTPUT_TAIL_BYTES`] of the output (its end), and its full size before the cut.
    pub output_tail: String,
    pub output_bytes: usize,
    /// The error text of a failed call, at most [`ERROR_TEXT_BYTES`].
    pub error: Option<String>,
    /// The exit code a shell result reported (`[Command exited with code N]`), when it did.
    pub exit_code: Option<i64>,
}

/// A line the harness computed about a tool input (a blank key, an unknown key for a known tool, an input that is not an object).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Anomaly {
    /// The number of the call ([`ToolRecord::n`]).
    pub call: u64,
    pub tool: String,
    /// One plain line, for example `input has a blank key`.
    pub line: String,
}

/// Counters the recorder adds to [`AttemptFacts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderCounters {
    /// Tool calls started in the run (all attempts and sessions).
    pub calls: u64,
    /// Calls at the end of the run with the same input, in a row (the input compared whole, not the cut title).
    pub same_input_tail: u32,
    /// Calls at the end of the run whose recorded verdict is `Failed` or `TimedOut`, in a row.
    pub bad_tail: u32,
}

/// What the caller knows about the stop and the recorder does not: the held outcome and its words, the stop reason, the engine session,
/// the run's age. Every field is optional text or a number; nothing is judged here.
#[derive(Clone, Debug, Default)]
pub struct StopContext {
    /// The `DirectOutcome` wire word of the stop, e.g. `stopped_engine_cancelled`.
    pub outcome: Option<String>,
    pub label: Option<String>,
    pub detail: Option<String>,
    /// The ACP stop reason, e.g. `cancelled`.
    pub stop_reason: Option<String>,
    /// 1 for the first engine session of the run, 2 after a fresh session.
    pub session: u32,
    pub run_s: u64,
    pub files_changed: Option<u32>,
    pub model: Option<String>,
    /// The message being written when the stop came (the text since the last tool call); it counts as the newest message.
    pub final_message: Option<String>,
}

/// The digest of one stop. [`StopDigest::to_json`] is at most [`STOP_DIGEST_MAX_BYTES`].
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopDigest {
    pub schema: u32,
    /// 1 for the first stop of the run, then 2 and up.
    pub n: u32,
    pub harness: DigestHarness,
    pub engine_counters: DigestEngineCounters,
    pub recorder_counters: RecorderCounters,
    pub anomalies: Vec<Anomaly>,
    pub messages: Vec<String>,
    /// The calls that fit, oldest first.
    pub calls: Vec<ToolRecord>,
    /// Older calls left out to stay inside the limit (0 when all fit).
    pub dropped_calls: u32,
    pub failed_test_tail: Option<String>,
}

/// The harness facts at the head of the digest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestHarness {
    pub outcome: Option<String>,
    pub label: Option<String>,
    pub detail: Option<String>,
    pub stop_reason: Option<String>,
    pub session: u32,
    pub run_s: u64,
    pub files_changed: Option<u32>,
    pub model: Option<String>,
}

/// [`AttemptFacts`] as the digest carries them (PM3's counters, not recomputed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestEngineCounters {
    pub tool_calls: u64,
    pub completed: u64,
    pub failed_tail: u32,
    pub identical_tail: u32,
}

impl StopDigest {
    /// The digest as compact JSON, at most [`STOP_DIGEST_MAX_BYTES`] bytes.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The digest as a JSON value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Largest slice of an output that is read (its end) before redaction, and of an input (its start): a bound on work, not on what is kept.
const PRE_REDACT_OUTPUT_BYTES: usize = 256 * 1024;
const PRE_REDACT_INPUT_BYTES: usize = 64 * 1024;
/// Longest piece of a context string (outcome, label, detail, model) the digest keeps, in characters.
const CTX_CHARS: usize = 600;

/// The parameters of the tools of the pinned engine (3.0.71), read from the tool specs of its Bedrock requests. A tool that is not
/// listed is checked for blank keys and for an input that is not an object only.
struct ToolSpec {
    name: &'static str,
    allowed: &'static [&'static str],
    required: &'static [&'static str],
}

const KNOWN_TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "apply_patch",
        allowed: &["input"],
        required: &["input"],
    },
    ToolSpec {
        name: "editor",
        allowed: &["insert_line", "new_text", "old_text", "path"],
        required: &["path", "new_text"],
    },
    ToolSpec {
        name: "fetch_web_content",
        allowed: &["requests"],
        required: &["requests"],
    },
    ToolSpec {
        name: "read_files",
        allowed: &["files"],
        required: &["files"],
    },
    ToolSpec {
        name: "run_commands",
        allowed: &["commands"],
        required: &["commands"],
    },
    ToolSpec {
        name: "search_codebase",
        allowed: &["queries"],
        required: &["queries"],
    },
    ToolSpec {
        name: "spawn_agent",
        allowed: &["systemPrompt", "task"],
        required: &["systemPrompt", "task"],
    },
];

/// Words in a command that make it a test run (lowercase).
const TEST_WORDS: &[&str] = &[
    "pytest",
    "py.test",
    "cargo test",
    "cargo nextest",
    "npm test",
    "npm run test",
    "yarn test",
    "pnpm test",
    "go test",
    "mvn test",
    "mvn -q test",
    "mvn verify",
    "gradle test",
    "gradlew test",
    "unittest",
    "jest",
    "vitest",
    "mocha",
    "rspec",
    "phpunit",
    "dotnet test",
    "make test",
    "ctest",
    "tox",
    "nox",
    "bats ",
];

/// Names whose value, after `=` or `:`, is a secret (lowercase).
const SECRET_NAMES: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "credential",
    "private_key",
    "access_key",
];

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn head_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The longest suffix of `s` that is at most `max` bytes and starts on a character boundary.
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn head_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `text` with the value of every `name=value`, `name: value`, `"name": "value"` secret (value of 6 or more characters) and every
/// `Bearer <token>` replaced by `<redacted>`. A name inside a longer word (`tokenizer`, `PASSWORD_FILE`) is not a secret name.
fn redact_named_secrets(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // name = value
    let mut pos = 0;
    while pos < text.len() {
        let next = SECRET_NAMES
            .iter()
            .filter_map(|n| lower[pos..].find(n).map(|k| (pos + k, n.len())))
            .min_by_key(|(k, _)| *k);
        let Some((at, len)) = next else { break };
        let mut j = at + len;
        if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
            j += 1;
        }
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j < bytes.len() && (bytes[j] == b'=' || bytes[j] == b':') {
            j += 1;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                j += 1;
            }
            let start = j;
            while j < bytes.len()
                && !matches!(
                    bytes[j],
                    b' ' | b'\n' | b'\r' | b'\t' | b'"' | b'\'' | b',' | b'}' | b';' | b')' | b'&'
                )
            {
                j += 1;
            }
            if j - start >= 6 && &text[start..j] != "<redacted>" {
                spans.push((start, j));
            }
            pos = j.max(at + len);
        } else {
            pos = at + len;
        }
    }
    // Bearer <token>
    let mut pos = 0;
    while let Some(k) = lower[pos..].find("bearer ") {
        let start = pos + k + "bearer ".len();
        let mut j = start;
        while j < bytes.len()
            && (bytes[j].is_ascii_alphanumeric()
                || matches!(bytes[j], b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-'))
        {
            j += 1;
        }
        if j - start >= 16 && &text[start..j] != "<redacted>" {
            spans.push((start, j));
        }
        pos = j.max(start);
    }
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (a, b) in spans {
        if a < cursor {
            continue;
        }
        out.push_str(&text[cursor..a]);
        out.push_str("<redacted>");
        cursor = b;
    }
    out.push_str(&text[cursor..]);
    out
}

/// `[Command exited with code N]` in the output: the first non-zero code, else the last code seen.
fn exit_code_of(text: &str) -> Option<i64> {
    const MARK: &str = "[Command exited with code ";
    let mut last = None;
    let mut rest = text;
    while let Some(k) = rest.find(MARK) {
        let after = &rest[k + MARK.len()..];
        let end = after.find(']').unwrap_or(after.len());
        if let Ok(code) = after[..end].trim().parse::<i64>() {
            if code != 0 {
                return Some(code);
            }
            last = Some(code);
        }
        rest = &after[end..];
    }
    last
}

/// True when the output says the shell cut the command (`Command timed out after 30000ms`).
fn timed_out(text: &str) -> bool {
    text.find("Command timed out after ")
        .map(|k| &text[k + "Command timed out after ".len()..])
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// What the result of a call says, read from its `rawOutput` or its `content`.
#[derive(Debug, Default)]
struct Output {
    text: String,
    /// An item says `success: false`, carries an `error`, or the text starts with `Error:`.
    failed: bool,
    error: Option<String>,
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn read_item(item: &Value, out: &mut Output, parts: &mut Vec<String>) {
    let Value::Object(m) = item else {
        parts.push(value_text(item));
        return;
    };
    let mut text = None;
    for key in ["result", "output", "stdout", "content", "text", "message"] {
        if let Some(v) = m.get(key).filter(|v| !v.is_null()) {
            let t = value_text(v);
            if !t.is_empty() {
                text = Some(t);
                break;
            }
        }
    }
    if let Some(err) = m.get("error").filter(|e| !e.is_null()) {
        let e = value_text(err);
        if !e.trim().is_empty() {
            out.failed = true;
            out.error = Some(match out.error.take() {
                Some(prev) => format!("{prev}; {e}"),
                None => e,
            });
        }
    }
    if m.get("success").and_then(Value::as_bool) == Some(false) {
        out.failed = true;
    }
    parts.push(text.unwrap_or_else(|| {
        if m.contains_key("error") {
            String::new()
        } else {
            item.to_string()
        }
    }));
}

fn read_structured(v: &Value) -> Output {
    let mut out = Output::default();
    let mut parts = Vec::new();
    match v {
        Value::Array(items) => items
            .iter()
            .for_each(|i| read_item(i, &mut out, &mut parts)),
        other => read_item(other, &mut out, &mut parts),
    }
    out.text = parts.join("\n");
    out
}

/// The result of a call from a `rawOutput` value: a JSON array or object (or a string holding one) of result items, or plain text.
fn read_raw_output(raw: &Value) -> Output {
    match raw {
        Value::Null => Output::default(),
        Value::String(s) => {
            let t = s.trim_start();
            if (t.starts_with('[') || t.starts_with('{'))
                && let Ok(v) = serde_json::from_str::<Value>(t)
                && (v.is_array() || v.is_object())
            {
                return read_structured(&v);
            }
            plain(s)
        }
        v @ (Value::Array(_) | Value::Object(_)) => read_structured(v),
        other => plain(&other.to_string()),
    }
}

fn plain(s: &str) -> Output {
    let t = s.trim_start();
    Output {
        text: s.to_string(),
        failed: t.starts_with("Error:") || t.starts_with("ERROR:"),
        error: None,
    }
}

/// The result of a call from its `content` blocks (ACP): text blocks keep their text, a diff keeps its path and never the text of the file.
fn read_content(content: &Value) -> Output {
    let blocks: Vec<&Value> = match content {
        Value::Array(a) => a.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    };
    let mut parts = Vec::new();
    for b in blocks {
        if b.get("type").and_then(Value::as_str) == Some("diff") {
            let path = b.get("path").and_then(Value::as_str).unwrap_or("?");
            parts.push(format!("diff {path}"));
        } else if let Some(t) = b
            .pointer("/content/text")
            .or_else(|| b.get("text"))
            .and_then(Value::as_str)
        {
            parts.push(t.to_string());
        }
    }
    plain(&parts.join("\n"))
}

/// The name of the tool from a title such as `run_commands: ls`; empty when the title has no such prefix.
fn tool_of(title: &str) -> String {
    match title.split_once(':') {
        Some((n, _))
            if !n.is_empty()
                && n.len() <= 40
                && n.starts_with(|c: char| c.is_ascii_lowercase())
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') =>
        {
            n.to_string()
        }
        _ => String::new(),
    }
}

fn fingerprint(input: &Value, title: &str) -> u64 {
    let mut h = Sha256::new();
    if input.is_null() {
        h.update(title.as_bytes());
    } else {
        h.update(input.to_string().as_bytes());
    }
    let d = h.finalize();
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// One ring slot: the record plus what the recorder keeps privately about it.
#[derive(Clone, Debug)]
struct Entry {
    rec: ToolRecord,
    fp: u64,
    structured_failed: bool,
    timed_out: bool,
    test_like: bool,
}

/// The bounded record of the run's tool calls and messages. See the module documentation.
#[derive(Clone, Debug, Default)]
pub struct Recorder {
    /// The run's exact secret values, read at every write: a set that the credential store grows as the credentials rotate.
    secrets: SecretSet,
    /// `session/prompt`s begun so far; the attempt number of a new call (calls before any prompt belong to attempt 1).
    begun: u32,
    /// Calls started in the run.
    next_n: u64,
    ring: VecDeque<Entry>,
    messages: VecDeque<String>,
    anomalies: VecDeque<Anomaly>,
    /// The tail of the output of the last test run that failed, until a later test run ends well.
    test_tail: Option<String>,
}

impl Recorder {
    /// A recorder that redacts these exact secret values (those under 8 characters are ignored, as in `redact_known`) in everything it
    /// stores, besides the key IDs and `name=value` secrets it always redacts.
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            secrets: SecretSet::new(secrets),
            ..Self::default()
        }
    }

    /// Replaces the secret values with a set of its own holding `secrets` (the collector's `with_secrets`).
    pub fn set_secrets(&mut self, secrets: Vec<String>) {
        self.secrets = SecretSet::new(secrets);
    }

    /// Redacts with the run's shared set from now on (the collector's `with_secret_set`): a secret a refresh mints after this call is
    /// in the set when the next write happens.
    pub fn set_secret_set(&mut self, set: SecretSet) {
        self.secrets = set;
    }

    /// Everything the recorder stores goes through this: the session's secret values as the set holds them now, AWS key IDs and
    /// `name=value` secrets.
    fn redact(&self, text: &str) -> String {
        redact_named_secrets(&redact_key_ids(&self.secrets.redact(text)))
    }

    /// A message of the engine as the record keeps it: control characters removed, redacted, trimmed, at most [`MESSAGE_BYTES`] (the end is kept).
    pub fn message_text(&self, raw: &str) -> String {
        let cleaned = clean_message(raw);
        let r = self.redact(cleaned.trim());
        tail_bytes(&r, MESSAGE_BYTES).trim_start().to_string()
    }

    /// A new `session/prompt` starts: the next calls belong to the next attempt. The ring and the messages are kept.
    pub fn begin_prompt(&mut self) {
        self.begun = self.begun.saturating_add(1);
    }

    /// A `session/update` of kind `tool_call` (`update` is the `update` member): opens a record.
    pub fn on_tool_call(&mut self, update: &Value) {
        self.next_n += 1;
        let n = self.next_n;
        let raw_title = update
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title_line: String = raw_title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let tool = tool_of(&title_line);
        let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
        let input_json = if input.is_null() {
            String::new()
        } else {
            input.to_string()
        };
        let input_keys = match &input {
            Value::Object(m) => m
                .keys()
                .take(KEYS_KEPT)
                .map(|k| head_chars(&self.redact(k), KEY_NAME_CHARS))
                .collect(),
            _ => Vec::new(),
        };
        let test_like = {
            let probe = head_bytes(&input_json, 4_096).to_ascii_lowercase();
            TEST_WORDS.iter().any(|w| probe.contains(w))
        };
        let rec = ToolRecord {
            n,
            attempt: self.begun.max(1),
            id: head_chars(
                &self.redact(
                    update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                64,
            ),
            tool: tool.clone(),
            kind: head_chars(
                update
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                40,
            ),
            title: head_chars(&self.redact(&title_line), TITLE_CHARS),
            engine_status: head_chars(
                update
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                24,
            ),
            verdict: CallVerdict::NoResult,
            input_head: head_bytes(
                &self.redact(head_bytes(&input_json, PRE_REDACT_INPUT_BYTES)),
                INPUT_HEAD_BYTES,
            )
            .to_string(),
            input_bytes: input_json.len(),
            input_keys,
            output_tail: String::new(),
            output_bytes: 0,
            error: None,
            exit_code: None,
        };
        self.check_input(n, &tool, &input);
        let mut entry = Entry {
            fp: fingerprint(&input, &title_line),
            rec,
            structured_failed: false,
            timed_out: false,
            test_like,
        };
        // Some engines send the final status with the call itself.
        if matches!(entry.rec.engine_status.as_str(), "completed" | "failed") {
            self.settle(&mut entry, update);
        }
        self.ring.push_back(entry);
        while self.ring.len() > RING_CALLS {
            self.ring.pop_front();
        }
    }

    /// A `session/update` of kind `tool_call_update`: closes or updates the record with the same `toolCallId` (an update for a call that
    /// is no longer in the ring is ignored).
    pub fn on_tool_call_update(&mut self, update: &Value) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let id = head_chars(&self.redact(id), 64);
        let Some(idx) = self.ring.iter().rposition(|e| e.rec.id == id) else {
            return;
        };
        let mut entry = self.ring[idx].clone();
        if let Some(status) = update.get("status").and_then(Value::as_str) {
            entry.rec.engine_status = head_chars(status, 24);
        }
        self.settle(&mut entry, update);
        self.ring[idx] = entry;
    }

    /// Reads the output of an update into the entry and, when the engine's status is final, decides the verdict.
    fn settle(&mut self, entry: &mut Entry, update: &Value) {
        let out = if let Some(raw) = update.get("rawOutput").filter(|v| !v.is_null()) {
            Some(read_raw_output(raw))
        } else {
            update.get("content").map(read_content)
        };
        let mut full_tail: Option<String> = None;
        if let Some(out) = out {
            let text = tail_bytes(&out.text, PRE_REDACT_OUTPUT_BYTES);
            let redacted = self.redact(text);
            entry.rec.output_bytes = out.text.len();
            entry.rec.output_tail = tail_bytes(&redacted, OUTPUT_TAIL_BYTES)
                .trim_start()
                .to_string();
            entry.rec.exit_code = exit_code_of(&out.text);
            entry.timed_out = timed_out(&out.text);
            entry.structured_failed = out.failed;
            entry.rec.error = out
                .error
                .as_deref()
                .map(|e| head_bytes(&self.redact(e), ERROR_TEXT_BYTES).to_string());
            full_tail = Some(
                tail_bytes(&redacted, TEST_TAIL_BYTES)
                    .trim_start()
                    .to_string(),
            );
        }
        let status = entry.rec.engine_status.as_str();
        if !matches!(status, "completed" | "failed") {
            return;
        }
        let bad_exit = entry.rec.exit_code.is_some_and(|c| c != 0);
        entry.rec.verdict = if entry.timed_out {
            CallVerdict::TimedOut
        } else if status == "failed" || entry.structured_failed || bad_exit {
            CallVerdict::Failed
        } else {
            CallVerdict::Ok
        };
        if entry.rec.verdict != CallVerdict::Ok && entry.rec.error.is_none() {
            let basis = if entry.rec.output_tail.is_empty() {
                "the engine reported the call failed".to_string()
            } else {
                entry.rec.output_tail.clone()
            };
            entry.rec.error = Some(head_bytes(&basis, ERROR_TEXT_BYTES).to_string());
        }
        if entry.test_like {
            self.test_tail = match entry.rec.verdict {
                CallVerdict::Ok => None,
                _ => full_tail.or_else(|| {
                    (!entry.rec.output_tail.is_empty()).then(|| entry.rec.output_tail.clone())
                }),
            };
        }
    }

    /// Anomaly lines about one tool input (rules in Rust, not a reading of raw text).
    fn check_input(&mut self, n: u64, tool: &str, input: &Value) {
        let mut lines: Vec<String> = Vec::new();
        match input {
            Value::Null => {}
            Value::Object(m) if m.is_empty() => lines.push("empty input".into()),
            Value::Object(m) => {
                let blank = m.keys().filter(|k| k.trim().is_empty()).count();
                match blank {
                    0 => {}
                    1 => lines.push("input has a blank key".into()),
                    k => lines.push(format!("input has {k} blank keys")),
                }
                if let Some(spec) = KNOWN_TOOLS.iter().find(|t| t.name == tool) {
                    for k in m.keys().filter(|k| !k.trim().is_empty()) {
                        if !spec.allowed.contains(&k.as_str()) {
                            lines.push(format!(
                                "input has an unknown key \"{}\" for {tool}",
                                head_chars(&self.redact(k), KEY_NAME_CHARS)
                            ));
                        }
                    }
                    for r in spec.required {
                        if !m.contains_key(*r) {
                            lines.push(format!("input is missing required key \"{r}\" of {tool}"));
                        }
                    }
                }
            }
            other => {
                let kind = match other {
                    Value::String(_) => "a string",
                    Value::Array(_) => "an array",
                    Value::Number(_) => "a number",
                    Value::Bool(_) => "a boolean",
                    _ => "not an object",
                };
                lines.push(format!("input is not an object (it is {kind})"));
            }
        }
        for line in lines {
            self.anomalies.push_back(Anomaly {
                call: n,
                tool: tool.to_string(),
                line,
            });
        }
        while self.anomalies.len() > ANOMALIES_KEPT {
            self.anomalies.pop_front();
        }
    }

    /// A finished message of the engine (the text between two tool calls).
    pub fn on_message(&mut self, text: &str) {
        let t = self.message_text(text);
        if t.is_empty() {
            return;
        }
        self.messages.push_back(t);
        while self.messages.len() > MESSAGES_KEPT {
            self.messages.pop_front();
        }
    }

    /// The last [`NOTES_LISTED`] calls as title and status, oldest first.
    pub fn tool_notes(&self) -> Vec<ToolNote> {
        let skip = self.ring.len().saturating_sub(NOTES_LISTED);
        self.ring
            .iter()
            .skip(skip)
            .map(|e| ToolNote {
                title: e.rec.title.clone(),
                status: e.rec.verdict.word().to_string(),
            })
            .collect()
    }

    /// The last [`MESSAGES_KEPT`] messages, oldest first, each at most [`MESSAGE_BYTES`] (the tail is kept), redacted.
    pub fn last_messages(&self) -> Vec<String> {
        self.messages.iter().cloned().collect()
    }

    /// The tail (at most [`TEST_TAIL_BYTES`]) of the output of the last failed call whose command looks like a test run, when the
    /// output was kept; `None` when there is none or a later test run ended well.
    pub fn failed_test_tail(&self) -> Option<String> {
        self.test_tail.clone()
    }

    /// The error text of the newest failed call, at most [`LAST_ERROR_CHARS`] characters, on one line: the needs-input card's
    /// `evidence.lastError`.
    pub fn last_error(&self) -> Option<String> {
        self.ring
            .iter()
            .rev()
            .filter(|e| matches!(e.rec.verdict, CallVerdict::Failed | CallVerdict::TimedOut))
            .find_map(|e| e.rec.error.as_deref())
            .map(|e| head_chars(&scrub_detail(e), LAST_ERROR_CHARS))
            .filter(|e| !e.is_empty())
    }

    /// The anomaly lines of the calls still in the ring, oldest first.
    pub fn anomalies(&self) -> Vec<Anomaly> {
        let oldest = self.ring.front().map_or(u64::MAX, |e| e.rec.n);
        self.anomalies
            .iter()
            .filter(|a| a.call >= oldest)
            .cloned()
            .collect()
    }

    /// The calls in the ring, oldest first (at most [`RING_CALLS`]).
    pub fn calls(&self) -> Vec<ToolRecord> {
        self.ring.iter().map(|e| e.rec.clone()).collect()
    }

    pub fn counters(&self) -> RecorderCounters {
        let same = match self.ring.back() {
            Some(last) => self
                .ring
                .iter()
                .rev()
                .take_while(|e| e.fp == last.fp)
                .count(),
            None => 0,
        };
        let bad = self
            .ring
            .iter()
            .rev()
            .take_while(|e| matches!(e.rec.verdict, CallVerdict::Failed | CallVerdict::TimedOut))
            .count();
        RecorderCounters {
            calls: self.next_n,
            same_input_tail: u32::try_from(same).unwrap_or(u32::MAX),
            bad_tail: u32::try_from(bad).unwrap_or(u32::MAX),
        }
    }

    /// The digest of stop number `n`. `facts` are the collector's [`AttemptFacts`] of the attempt that just ended; `ctx` is what the
    /// driver knows. Whole records are left out from the oldest end until the JSON fits [`STOP_DIGEST_MAX_BYTES`]; a record is never
    /// cut and `dropped_calls` says how many went. Older records that no longer fit whole are first shortened (fields of at most
    /// [`COMPACT_FIELD_BYTES`]) before any is left out.
    pub fn stop_digest(&self, n: u32, facts: &AttemptFacts, ctx: &StopContext) -> StopDigest {
        self.stop_digest_within(n, facts, ctx, STOP_DIGEST_MAX_BYTES)
    }

    /// [`Self::stop_digest`] with a limit other than [`STOP_DIGEST_MAX_BYTES`] (smaller, in tests). If even the fixed part is over the limit,
    /// the messages go first (oldest first), then the failed test tail, then the anomaly lines (oldest first); the calls are filled in last.
    pub fn stop_digest_within(
        &self,
        n: u32,
        facts: &AttemptFacts,
        ctx: &StopContext,
        max_bytes: usize,
    ) -> StopDigest {
        let ctx_text = |s: &Option<String>, max: usize| {
            s.as_deref()
                .map(|t| head_chars(&self.redact(&scrub_detail_long(t)), max))
                .filter(|t| !t.is_empty())
        };
        let mut messages = self.last_messages();
        if let Some(fm) = ctx.final_message.as_deref() {
            let fm = self.message_text(fm);
            if !fm.is_empty() && messages.last() != Some(&fm) {
                messages.push(fm);
            }
        }
        let skip = messages.len().saturating_sub(MESSAGES_KEPT);
        let messages: Vec<String> = messages.into_iter().skip(skip).collect();
        let mut digest = StopDigest {
            schema: 1,
            n,
            harness: DigestHarness {
                outcome: ctx_text(&ctx.outcome, 64),
                label: ctx_text(&ctx.label, 200),
                detail: ctx_text(&ctx.detail, CTX_CHARS),
                stop_reason: ctx_text(&ctx.stop_reason, 64),
                session: ctx.session.max(1),
                run_s: ctx.run_s,
                files_changed: ctx.files_changed,
                model: ctx_text(&ctx.model, 200),
            },
            engine_counters: DigestEngineCounters {
                tool_calls: facts.tool_calls,
                completed: facts.completed,
                failed_tail: facts.failed_tail,
                identical_tail: facts.identical_tail,
            },
            recorder_counters: self.counters(),
            anomalies: self.anomalies(),
            messages,
            calls: Vec::new(),
            dropped_calls: 0,
            failed_test_tail: self.failed_test_tail(),
        };
        // The fixed part always fits in practice (at most about 32 KB); if it ever did not, the least important parts go first.
        while digest.to_json().len() > max_bytes {
            if !digest.messages.is_empty() {
                digest.messages.remove(0);
            } else if digest.failed_test_tail.is_some() {
                digest.failed_test_tail = None;
            } else if !digest.anomalies.is_empty() {
                digest.anomalies.remove(0);
            } else {
                break;
            }
        }
        let base = digest.to_json().len();
        let room = max_bytes.saturating_sub(base);
        let whole_room = room / 10 * 6;
        let ring = self.calls();
        let mut kept: Vec<ToolRecord> = Vec::new();
        let mut used = 0usize;
        let mut idx = ring.len();
        while idx > 0 {
            let size = serde_json::to_string(&ring[idx - 1]).map_or(usize::MAX, |s| s.len() + 1);
            if (!kept.is_empty() && used + size > whole_room) || used + size > room {
                break;
            }
            kept.push(ring[idx - 1].clone());
            used += size;
            idx -= 1;
        }
        while idx > 0 {
            let compact = compact_record(&ring[idx - 1]);
            let size = serde_json::to_string(&compact).map_or(usize::MAX, |s| s.len() + 1);
            if used + size > room {
                break;
            }
            kept.push(compact);
            used += size;
            idx -= 1;
        }
        kept.reverse();
        digest.dropped_calls = u32::try_from(idx).unwrap_or(u32::MAX);
        digest.calls = kept;
        while digest.to_json().len() > max_bytes && !digest.calls.is_empty() {
            digest.calls.remove(0);
            digest.dropped_calls += 1;
        }
        digest
    }
}

/// A text of the harness (outcome, detail) on one line.
fn scrub_detail_long(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// An older record cut to fit: input, output and error shortened to [`COMPACT_FIELD_BYTES`].
fn compact_record(r: &ToolRecord) -> ToolRecord {
    ToolRecord {
        input_head: head_bytes(&r.input_head, COMPACT_FIELD_BYTES).to_string(),
        output_tail: tail_bytes(&r.output_tail, COMPACT_FIELD_BYTES).to_string(),
        error: r
            .error
            .as_deref()
            .map(|e| head_bytes(e, COMPACT_FIELD_BYTES).to_string()),
        ..r.clone()
    }
}
