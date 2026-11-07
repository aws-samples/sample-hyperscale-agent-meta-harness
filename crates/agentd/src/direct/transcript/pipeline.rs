//! The record pipeline of the transcript writer: the JSON-RPC messages
//! of the engine connection in, numbered record lines out. No I/O and no clock of its own: the caller gives it the instants.
//!
//! What a message becomes: a `session/update` becomes a record named by its update word; `session/prompt` and its reply are `prompt` and
//! `prompt_result`; the reply of `session/new` or `session/load` is a `session` record that carries the `initialize` result; a cancel,
//! a permission request and agentd's answer have records of their own; anything else is an `opaque` record (name, size, a payload cut at
//! 4 KiB). Never stored: thoughts, history the engine replays while a session loads, messages of an abandoned session, a configuration
//! update that did not change, binary output (replaced by its size) and the output of a tool that read a credential file.
//!
//! Message text is coalesced: the chunks of one message gather and are written as a fragment at 8 KiB, at 2 s, when another message
//! follows and at the end. Each write goes through a [`Stream`] that holds back a window, so a secret split across two fragments never
//! reaches a record. Every other payload goes through [`prepare`]: redacted whole, then cut to the record cap.

use crate::acp::Dir;
use crate::direct::transcript::finish::{Prepared, names_credential_file, prepare};
use crate::direct::transcript::settings::Settings;
use crate::time::format_rfc3339;
use harness_core::redact::{Redactor, Stream};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// How many requests of each direction wait for their reply in the table; the oldest is forgotten past it.
const MAX_PENDING: usize = 512;
/// How many tool calls are remembered for the credential-file rule.
const MAX_TOOLS: usize = 4096;
/// The longest `kind` or update word kept.
const MAX_WORD: usize = 64;

/// One JSON-RPC message of the engine connection, as the capture queue hands it over.
#[derive(Clone, Debug)]
pub struct WireMsg {
    pub dir: Dir,
    pub value: Value,
    /// The message belongs to a session the run abandoned.
    pub stale: bool,
    /// Seconds since the epoch when the module saw it.
    pub at: i64,
}

/// One finished record: its number, its kind word and its JSON object (no newline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub n: u64,
    pub kind: String,
    pub bytes: Vec<u8>,
}

/// What the pipeline left out, cut or lost, for the completeness summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub records: u64,
    /// Thoughts, replayed history, binary output and credential-file output.
    pub excluded: u64,
    pub truncated: u64,
    pub suppressed: u64,
    /// Wire messages dropped under load.
    pub shed: u64,
    /// Messages of an abandoned session that were skipped.
    pub stale: u64,
    /// Updates that came outside an open prompt (stored with `turn_id` null).
    pub unattributed: u64,
}

/// The prompt a record belongs to.
#[derive(Clone, Debug, Default)]
enum Turn {
    /// Not decided yet: the message that made the record decides.
    #[default]
    Unset,
    /// The JSON-RPC id of the open `session/prompt`, as it came.
    Open(Value),
    /// No prompt of the record's session was open: stored as `turn_id: null`.
    Closed,
}

/// A `session/prompt` that has been sent and not answered.
#[derive(Debug)]
struct OpenPrompt {
    key: String,
    id: Value,
    session: String,
}

/// The most prompts kept open at once: there is one at a time, so more is a lost answer, and the oldest goes.
const MAX_OPEN: usize = 16;

/// A record before it is prepared and numbered.
struct Raw {
    at: i64,
    dir: &'static str,
    kind: String,
    data: Value,
    sensitive: bool,
    /// The text went through the stream window already.
    redacted: bool,
    turn: Turn,
}

/// A bounded table from a request id to its method.
#[derive(Default)]
struct Methods {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl Methods {
    fn put(&mut self, id: String, method: &str) {
        if self.map.insert(id.clone(), method.to_string()).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > MAX_PENDING {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    fn take(&mut self, id: &str) -> Option<String> {
        let m = self.map.remove(id)?;
        self.order.retain(|x| x != id);
        Some(m)
    }
}

/// The message in progress.
struct Message {
    session: String,
    engine_id: Option<String>,
    id: String,
    buf: String,
    stream: Stream,
    /// Members of the first chunk besides the content: kept on every fragment.
    extra: Map<String, Value>,
    deadline: Instant,
    /// The prompt the first chunk came in: the message keeps it when its text is written later.
    turn: Turn,
}

#[derive(Debug)]
pub struct Pipeline {
    s: Settings,
    next_n: u64,
    stats: Stats,
    last_at: i64,
    out_methods: Methods,
    in_methods: Methods,
    initialize: Option<Value>,
    /// The id of the `session/load` request in flight: updates before its reply are history.
    replay: Option<String>,
    config_hash: HashMap<String, [u8; 32]>,
    tools: HashMap<String, bool>,
    tool_order: VecDeque<String>,
    msg: Option<Message>,
    msg_seq: u64,
    /// Prompts sent and not answered, oldest first.
    open: Vec<OpenPrompt>,
    /// The prompt the message being interpreted belongs to.
    cur_turn: Turn,
}

impl std::fmt::Debug for Methods {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Methods")
    }
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Message")
    }
}

fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= MAX_WORD
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        other => format!("n:{other}"),
    }
}

/// `v` as stored in an opaque record: itself when small, else the first `cut` bytes of its JSON text.
fn cut_payload(v: &Value, cut: usize) -> Value {
    let text = serde_json::to_string(v).unwrap_or_default();
    if text.len() <= cut {
        return v.clone();
    }
    let mut end = cut;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(text[..end].to_string())
}

/// A gap record: a range of the conversation that is not shown in full, with its kind word and a reason.
pub fn gap_line(n: u64, at: i64, extra: Value) -> Line {
    let rec = json!({"n": n, "t": format_rfc3339(at), "dir": "in", "kind": "gap", "data": extra});
    Line {
        n,
        kind: "gap".to_string(),
        bytes: serde_json::to_vec(&rec).unwrap_or_default(),
    }
}

impl Pipeline {
    pub fn new(settings: Settings, now: Instant) -> Self {
        let _ = now;
        Self {
            s: settings,
            next_n: 0,
            stats: Stats::default(),
            last_at: 0,
            out_methods: Methods::default(),
            in_methods: Methods::default(),
            initialize: None,
            replay: None,
            config_hash: HashMap::new(),
            tools: HashMap::new(),
            tool_order: VecDeque::new(),
            msg: None,
            msg_seq: 0,
            open: Vec::new(),
            cur_turn: Turn::Closed,
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The number the next record takes.
    pub fn next_n(&self) -> u64 {
        self.next_n
    }

    /// When the message in progress must be written at the latest.
    pub fn deadline(&self) -> Option<Instant> {
        self.msg.as_ref().map(|m| m.deadline)
    }

    /// Interprets one message of the connection.
    pub fn feed(&mut self, r: &Redactor, now: Instant, m: WireMsg) -> Vec<Line> {
        self.last_at = m.at;
        if m.stale {
            self.stats.stale += 1;
            return Vec::new();
        }
        let turn = self.attribute(&m);
        self.cur_turn = turn.clone();
        let mut raws: Vec<Raw> = Vec::new();
        self.interpret(r, now, &m, &mut raws);
        for raw in &mut raws {
            if matches!(raw.turn, Turn::Unset) {
                raw.turn = turn.clone();
            }
        }
        raws.into_iter().map(|raw| self.number(r, raw)).collect()
    }

    /// The prompt a wire message belongs to, and the changes it makes to the set of open prompts. A prompt names itself, its answer
    /// closes it, an `initialize` request (a new engine process) closes them all; any other message takes the open prompt of the session
    /// it names, or the latest open prompt when it names none. A live `session/update` with no open prompt is counted.
    fn attribute(&mut self, m: &WireMsg) -> Turn {
        let v = &m.value;
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id").filter(|i| !i.is_null());
        let session = v.pointer("/params/sessionId").and_then(Value::as_str);
        let is_response = method.is_none()
            && id.is_some()
            && (v.get("result").is_some() || v.get("error").is_some());
        match (m.dir, method, id, is_response) {
            (Dir::ToEngine, Some("initialize"), _, _) => {
                self.open.clear();
                Turn::Closed
            }
            (Dir::ToEngine, Some("session/prompt"), Some(id), _) => {
                self.open.retain(|o| o.key != id_key(id));
                self.open.push(OpenPrompt {
                    key: id_key(id),
                    id: id.clone(),
                    session: session.unwrap_or_default().to_string(),
                });
                while self.open.len() > MAX_OPEN {
                    self.open.remove(0);
                }
                Turn::Open(id.clone())
            }
            (Dir::FromEngine, None, Some(id), true) => {
                match self.open.iter().position(|o| o.key == id_key(id)) {
                    Some(at) => Turn::Open(self.open.remove(at).id),
                    None => self.turn_of(None),
                }
            }
            (Dir::FromEngine, Some("session/update"), None, _) => {
                let turn = self.turn_of(session);
                let live = v.pointer("/params/update").is_some_and(Value::is_object)
                    && self.replay.is_none();
                if live && matches!(turn, Turn::Closed) {
                    self.stats.unattributed += 1;
                }
                turn
            }
            _ => self.turn_of(session),
        }
    }

    /// The open prompt of `session`, or the latest open prompt when the message names no session.
    fn turn_of(&self, session: Option<&str>) -> Turn {
        let found = match session {
            Some(s) => self.open.iter().rev().find(|o| o.session == s),
            None => self.open.last(),
        };
        found.map_or(Turn::Closed, |o| Turn::Open(o.id.clone()))
    }

    /// Writes the message in progress when its two seconds are up.
    pub fn tick(&mut self, r: &Redactor, now: Instant) -> Vec<Line> {
        let mut raws = Vec::new();
        if self.msg.as_ref().is_some_and(|m| now >= m.deadline) {
            self.fragment(r, now, false, &mut raws);
        }
        raws.into_iter().map(|raw| self.number(r, raw)).collect()
    }

    /// Writes the message in progress now, all but the window of text that is held back (a cancel or a flush request).
    pub fn flush(&mut self, r: &Redactor, now: Instant) -> Vec<Line> {
        let mut raws = Vec::new();
        self.fragment(r, now, false, &mut raws);
        raws.into_iter().map(|raw| self.number(r, raw)).collect()
    }

    /// The end of the conversation: the message in progress is written whole.
    pub fn finish(&mut self, r: &Redactor) -> Vec<Line> {
        let mut raws = Vec::new();
        self.end_message(r, Instant::now(), &mut raws);
        raws.into_iter().map(|raw| self.number(r, raw)).collect()
    }

    /// Wire messages were dropped under load: a gap record in their place, after the message in progress.
    pub fn note_shed(&mut self, r: &Redactor, count: u64, reason: &str, at: i64) -> Vec<Line> {
        self.last_at = at;
        self.stats.shed += count;
        let mut raws = Vec::new();
        self.end_message(r, Instant::now(), &mut raws);
        let mut lines: Vec<Line> = raws.into_iter().map(|raw| self.number(r, raw)).collect();
        let n = self.next_n;
        self.next_n += 1;
        self.stats.records += 1;
        lines.push(gap_line(
            n,
            at,
            json!({"kind": "shed", "count": count, "reason": reason}),
        ));
        lines
    }

    /// A gap record with the next number: a range of the conversation that is not shown in full.
    pub fn gap(&mut self, at: i64, data: Value) -> Line {
        let n = self.next_n;
        self.next_n += 1;
        self.stats.records += 1;
        gap_line(n, at, data)
    }

    /// The last record of a clean end (kind `end`, from agentd): the object that holds it says it is closed.
    pub fn end(&mut self, at: i64) -> Line {
        let n = self.next_n;
        self.next_n += 1;
        self.stats.records += 1;
        let mut data = json!({"reason": "final"});
        if self.stats.unattributed > 0 {
            data["unattributed"] = json!(self.stats.unattributed);
        }
        let rec =
            json!({"n": n, "t": format_rfc3339(at), "dir": "out", "kind": "end", "data": data});
        Line {
            n,
            kind: "end".to_string(),
            bytes: serde_json::to_vec(&rec).unwrap_or_default(),
        }
    }

    /// Prepares, numbers and serializes one raw record.
    fn number(&mut self, r: &Redactor, raw: Raw) -> Line {
        let Prepared {
            data,
            truncated,
            suppressed,
            excluded,
        } = prepare(r, &self.s, raw.data, raw.sensitive, raw.redacted);
        self.stats.truncated += u64::from(truncated);
        self.stats.suppressed += u64::from(suppressed);
        self.stats.excluded += u64::from(excluded);
        let n = self.next_n;
        self.next_n += 1;
        self.stats.records += 1;
        let mut rec = json!({"n": n, "t": format_rfc3339(raw.at), "dir": raw.dir, "kind": raw.kind, "data": data});
        match raw.turn {
            Turn::Open(id) => rec["turn_id"] = id,
            Turn::Closed => rec["turn_id"] = Value::Null,
            Turn::Unset => {}
        }
        Line {
            n,
            kind: raw.kind,
            bytes: serde_json::to_vec(&rec).unwrap_or_default(),
        }
    }

    fn raw(&self, dir: &'static str, kind: &str, data: Value) -> Raw {
        Raw {
            at: self.last_at,
            dir,
            kind: kind.to_string(),
            data,
            sensitive: false,
            redacted: false,
            turn: Turn::Unset,
        }
    }

    fn opaque(
        &self,
        dir: &'static str,
        label: (&str, &str),
        id: Option<&Value>,
        payload: &Value,
    ) -> Raw {
        let bytes = serde_json::to_vec(payload).map_or(0, |b| b.len());
        let mut data = Map::new();
        data.insert(
            label.0.to_string(),
            Value::String(label.1.chars().take(MAX_WORD).collect()),
        );
        if let Some(id) = id {
            data.insert("id".into(), id.clone());
        }
        data.insert("bytes".into(), json!(bytes));
        data.insert("payload".into(), cut_payload(payload, self.s.opaque_cut));
        self.raw(dir, "opaque", Value::Object(data))
    }

    // ---- interpretation -----------------------------------------------------------------------------------------------------------

    fn interpret(&mut self, r: &Redactor, now: Instant, m: &WireMsg, out: &mut Vec<Raw>) {
        let v = &m.value;
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id").filter(|i| !i.is_null());
        let is_response = method.is_none()
            && id.is_some()
            && (v.get("result").is_some() || v.get("error").is_some());
        match (m.dir, method, id, is_response) {
            (Dir::FromEngine, Some("session/update"), None, _) => self.update(r, now, v, out),
            (Dir::FromEngine, Some(method), Some(id), _) => {
                self.end_message(r, now, out);
                self.in_methods.put(id_key(id), method);
                if method == "session/request_permission" {
                    out.push(self.raw("in", "permission_request", json!({"id": id, "params": v.get("params").cloned().unwrap_or(Value::Null)})));
                } else {
                    out.push(self.opaque(
                        "in",
                        ("method", method),
                        Some(id),
                        v.get("params").unwrap_or(&Value::Null),
                    ));
                }
            }
            (Dir::FromEngine, _, Some(id), true) => {
                self.end_message(r, now, out);
                let asked = self.out_methods.take(&id_key(id));
                self.reply(id, v, asked.as_deref(), out);
            }
            (Dir::FromEngine, method, _, _) => {
                self.end_message(r, now, out);
                out.push(self.opaque(
                    "in",
                    ("method", method.unwrap_or("unknown")),
                    id,
                    v.get("params").unwrap_or(v),
                ));
            }
            (Dir::ToEngine, Some(method), Some(id), _) => {
                self.end_message(r, now, out);
                self.out_methods.put(id_key(id), method);
                match method {
                    "session/prompt" => {
                        let p = v.get("params");
                        out.push(self.raw(
                            "out",
                            "prompt",
                            json!({"id": id, "sessionId": p.and_then(|p| p.get("sessionId")).cloned().unwrap_or(Value::Null), "prompt": p.and_then(|p| p.get("prompt")).cloned().unwrap_or(Value::Null)}),
                        ));
                    }
                    "session/load" => {
                        self.replay = Some(id_key(id));
                        out.push(self.opaque(
                            "out",
                            ("method", method),
                            Some(id),
                            v.get("params").unwrap_or(&Value::Null),
                        ));
                    }
                    _ => out.push(self.opaque(
                        "out",
                        ("method", method),
                        Some(id),
                        v.get("params").unwrap_or(&Value::Null),
                    )),
                }
            }
            (Dir::ToEngine, Some("session/cancel"), None, _) => {
                self.end_message(r, now, out);
                let sid = v
                    .pointer("/params/sessionId")
                    .cloned()
                    .unwrap_or(Value::Null);
                out.push(self.raw("out", "cancel", json!({"sessionId": sid})));
            }
            (Dir::ToEngine, _, Some(id), true) => {
                self.end_message(r, now, out);
                let asked = self.in_methods.take(&id_key(id));
                let mut data = Map::new();
                data.insert("id".into(), id.clone());
                for k in ["result", "error"] {
                    if let Some(x) = v.get(k) {
                        data.insert(k.into(), x.clone());
                    }
                }
                if asked.as_deref() == Some("session/request_permission") {
                    out.push(self.raw("out", "permission_response", Value::Object(data)));
                } else {
                    out.push(self.opaque(
                        "out",
                        ("method", asked.as_deref().unwrap_or("response")),
                        Some(id),
                        &Value::Object(data),
                    ));
                }
            }
            (Dir::ToEngine, method, _, _) => {
                self.end_message(r, now, out);
                out.push(self.opaque(
                    "out",
                    ("method", method.unwrap_or("unknown")),
                    id,
                    v.get("params").unwrap_or(v),
                ));
            }
        }
    }

    /// The reply of the engine to a request of agentd's.
    fn reply(&mut self, id: &Value, v: &Value, asked: Option<&str>, out: &mut Vec<Raw>) {
        let mut data = Map::new();
        data.insert("id".into(), id.clone());
        for k in ["result", "error"] {
            if let Some(x) = v.get(k) {
                data.insert(k.into(), x.clone());
            }
        }
        match asked {
            Some("session/prompt") => {
                out.push(self.raw("in", "prompt_result", Value::Object(data)))
            }
            Some(method @ ("session/new" | "session/load")) => {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                if method == "session/load" {
                    self.replay = None;
                }
                let mut d = Map::new();
                d.insert("id".into(), id.clone());
                d.insert("method".into(), Value::String(method.to_string()));
                d.insert(
                    "sessionId".into(),
                    result.get("sessionId").cloned().unwrap_or(Value::Null),
                );
                d.insert(
                    "result".into(),
                    if v.get("error").is_some() {
                        v["error"].clone()
                    } else {
                        result
                    },
                );
                if let Some(init) = &self.initialize {
                    d.insert("initialize".into(), init.clone());
                }
                out.push(self.raw("in", "session", Value::Object(d)));
            }
            Some("initialize") => {
                if let Some(res) = v.get("result") {
                    self.initialize = Some(res.clone());
                }
                out.push(self.opaque(
                    "in",
                    ("method", "initialize"),
                    Some(id),
                    &Value::Object(data),
                ));
            }
            other => out.push(self.opaque(
                "in",
                ("method", other.unwrap_or("response")),
                Some(id),
                &Value::Object(data),
            )),
        }
    }

    fn remember_tool(&mut self, id: &str, sensitive: bool) {
        if self.tools.insert(id.to_string(), sensitive).is_none() {
            self.tool_order.push_back(id.to_string());
        }
        while self.tool_order.len() > MAX_TOOLS {
            if let Some(old) = self.tool_order.pop_front() {
                self.tools.remove(&old);
            }
        }
    }

    fn update(&mut self, r: &Redactor, now: Instant, v: &Value, out: &mut Vec<Raw>) {
        let params = v.get("params");
        let upd = params
            .and_then(|p| p.get("update"))
            .filter(|u| u.is_object());
        let Some(upd) = upd else {
            self.end_message(r, now, out);
            out.push(self.opaque(
                "in",
                ("method", "session/update"),
                None,
                v.get("params").unwrap_or(v),
            ));
            return;
        };
        let session = params
            .and_then(|p| p.get("sessionId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let word = upd
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.replay.is_some() {
            // The engine replays the saved history while a session loads: it is in the chunks of the earlier writer.
            self.stats.excluded += 1;
            return;
        }
        if !is_word(word) {
            self.end_message(r, now, out);
            let label: String = word.chars().take(MAX_WORD).collect();
            out.push(self.opaque("in", ("variant", &label), None, upd));
            return;
        }
        match word {
            "agent_thought_chunk" => {
                // Thoughts are stored nowhere: not in a record, not in the message in progress.
                self.stats.excluded += 1;
            }
            "agent_message_chunk" => self.message_chunk(r, now, &session, upd, out),
            "tool_call" | "tool_call_update" => {
                self.end_message(r, now, out);
                let call = upd
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let named = names_credential_file(upd);
                let sensitive = named
                    || (word == "tool_call_update"
                        && self.tools.get(&call).copied().unwrap_or(false));
                if !call.is_empty() {
                    self.remember_tool(&call, sensitive);
                }
                let mut raw = self.raw("in", word, upd.clone());
                raw.sensitive = sensitive;
                out.push(raw);
            }
            "config_option_update" => {
                self.end_message(r, now, out);
                let digest: [u8; 32] =
                    Sha256::digest(serde_json::to_vec(upd).unwrap_or_default()).into();
                if self.config_hash.insert(session, digest) != Some(digest) {
                    out.push(self.raw("in", word, upd.clone()));
                }
            }
            "user_message_chunk"
            | "plan"
            | "available_commands_update"
            | "current_mode_update"
            | "session_info_update"
            | "usage_update" => {
                self.end_message(r, now, out);
                out.push(self.raw("in", word, upd.clone()));
            }
            other => {
                self.end_message(r, now, out);
                out.push(self.opaque("in", ("variant", other), None, upd));
            }
        }
    }

    // ---- message text ---------------------------------------------------------------------------------------------------------------

    fn message_chunk(
        &mut self,
        r: &Redactor,
        now: Instant,
        session: &str,
        upd: &Value,
        out: &mut Vec<Raw>,
    ) {
        let content = upd.get("content");
        let text = content
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str);
        let Some(text) = text else {
            self.end_message(r, now, out);
            out.push(self.raw("in", "agent_message_chunk", upd.clone()));
            return;
        };
        let engine_id = upd
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let same = self
            .msg
            .as_ref()
            .is_some_and(|m| m.session == session && m.engine_id == engine_id);
        if !same {
            self.end_message(r, now, out);
            self.msg_seq += 1;
            let mut extra = upd.as_object().cloned().unwrap_or_default();
            for k in ["content", "sessionUpdate", "messageId"] {
                extra.remove(k);
            }
            self.msg = Some(Message {
                session: session.to_string(),
                id: engine_id
                    .clone()
                    .unwrap_or_else(|| format!("w{}", self.msg_seq)),
                engine_id,
                buf: String::new(),
                stream: Stream::new(self.s.window, self.s.max_hold),
                extra,
                deadline: now + self.s.coalesce_every,
                turn: self.cur_turn.clone(),
            });
        }
        let Some(m) = self.msg.as_mut() else {
            return;
        };
        m.buf.push_str(text);
        if m.buf.len() >= self.s.coalesce_bytes {
            self.fragment(r, now, false, out);
        }
    }

    /// Writes what the message in progress can write now: all but the window, or everything at the end.
    fn fragment(&mut self, r: &Redactor, now: Instant, end: bool, out: &mut Vec<Raw>) {
        let every = self.s.coalesce_every;
        let at = self.last_at;
        let Some(m) = self.msg.as_mut() else {
            return;
        };
        let chunk = std::mem::take(&mut m.buf);
        let mut text = m.stream.push(r, &chunk);
        if end {
            text.push_str(&m.stream.finish(r));
        }
        m.deadline = now + every;
        if text.is_empty() {
            return;
        }
        let mut data = m.extra.clone();
        data.insert(
            "sessionUpdate".into(),
            Value::String("agent_message_chunk".into()),
        );
        data.insert("content".into(), json!({"type": "text", "text": text}));
        data.insert("messageId".into(), Value::String(m.id.clone()));
        out.push(Raw {
            at,
            dir: "in",
            kind: "agent_message_chunk".to_string(),
            data: Value::Object(data),
            sensitive: false,
            redacted: true,
            turn: m.turn.clone(),
        });
    }

    /// Another message follows (or the conversation ends): the message in progress is written whole.
    fn end_message(&mut self, r: &Redactor, now: Instant, out: &mut Vec<Raw>) {
        if self.msg.is_some() {
            self.fragment(r, now, true, out);
            self.msg = None;
        }
    }
}
