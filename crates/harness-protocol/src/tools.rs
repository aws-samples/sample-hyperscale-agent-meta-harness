//! The tool table of the harness's own tool server (protocol 1.40.0, wire B of the tool layer, trimmed to `code_search`).
//!
//! agentd serves a small MCP server on the loopback of the microVM, and the engine lists its tools from a settings file that names it. The table here
//! is what every party shares: the server answers `tools/list` from it, the profile of an engine registers the server, the run summary counts calls
//! by the names in it, and the live driver checks the three against each other. The crate does no I/O.
//!
//! What a name is. A tool name is a bare word (`code_search`): `^[a-z][a-z0-9_]{0,23}$` with no run of two underscores, because the engine joins
//! server and tool with two (`ham__code_search` in Cline) and the joined form must stay under 64 characters. A name carries no engine word. The
//! summary lists and counts the bare names; the stored events carry the engine's joined form.
//!
//! What is here in this window. One tool, `code_search`. `web_search` and `semantic_search` join with their own window and their own protocol
//! version: [`ToolName`] has one variant and [`TOOLS`] one entry until then, and nothing on the wire is a closed list of tools (the summary holds
//! plain words), so a reader built before the next window still reads a summary written after it.
//!
//! Descriptions are model-facing text. The engine sends every description and every input schema with every model request, so a description is at
//! most [`DESCRIPTION_MAX`] characters, the input schema carries no dialect line or title, and `tests/tools_table.rs` pins the SHA-256 of the whole
//! block: a change to any word of it is a change to the token cost of every request and needs a look at that cost.

use crate::export::canonical;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The MCP server name. An engine shows a tool as `<server>__<tool>` (Cline: `ham__code_search`).
pub const SERVER_NAME: &str = "ham";

/// A tool description is at most this many characters.
pub const DESCRIPTION_MAX: usize = 400;

/// The words an answer to a failed call may start with, after `error: `. The failure comes first in the text because an engine passes the whole
/// result to the model as text; a JSON-RPC error is reserved for protocol faults.
pub const RESULT_FAILURE_WORDS: &[&str] = &[
    "busy",
    "disabled",
    "refused",
    "timeout",
    "unavailable",
    "usage",
];

/// A summary lists at most this many tools and counts at most this many.
pub const MAX_TOOLS_LISTED: usize = 16;
/// The longest `reason` of a [`DirectToolsNote`].
pub const MAX_REASON_CHARS: usize = 40;
/// The longest `connect` word of a [`DirectToolsNote`].
pub const MAX_CONNECT_CHARS: usize = 24;

/// The longest pattern of `code_search`, in characters.
pub const MAX_PATTERN_CHARS: usize = 1_000;
/// The most entries in `paths` and in `glob`.
pub const MAX_LIST_ENTRIES: usize = 10;
/// The longest entry of `paths` or `glob`, in bytes.
pub const MAX_ENTRY_BYTES: usize = 1_024;
/// `maxResults` is 1 to this many; absent means [`DEFAULT_MAX_RESULTS`].
pub const MAX_RESULTS: u32 = 200;
/// `maxPerFile` is 1 to this many; absent means [`DEFAULT_MAX_PER_FILE`].
pub const MAX_PER_FILE: u32 = 50;
/// `context` is 0 to this many lines.
pub const MAX_CONTEXT: u32 = 5;
/// The number of matches returned when `maxResults` is absent.
pub const DEFAULT_MAX_RESULTS: u32 = 50;
/// The number of matches per file when `maxPerFile` is absent.
pub const DEFAULT_MAX_PER_FILE: u32 = 10;

/// A tool name, a reason or a count key: one plain word. See the module text for the rule.
pub fn is_tool_word(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=24).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !s.contains("__")
}

/// The tools of the table. One variant in this window; a tool joins with its window and a protocol version. Not serialized: the wire holds the
/// bare name as a string (see the module text), and `non_exhaustive` is left off on purpose, so that a new tool is a compile error in every match
/// on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolName {
    CodeSearch,
}

impl ToolName {
    /// Every tool, in table order.
    pub const ALL: &'static [ToolName] = &[ToolName::CodeSearch];

    /// The bare name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodeSearch => "code_search",
        }
    }

    /// The tool a bare name stands for; `None` for anything else, including a name the engine has prefixed.
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == word)
    }
}

/// One entry of the table: what a model is shown for a tool.
#[derive(Clone, Copy, Debug)]
pub struct ToolSpec {
    pub name: ToolName,
    /// At most [`DESCRIPTION_MAX`] characters.
    pub description: &'static str,
    /// The JSON Schema of the arguments, as an MCP `inputSchema`.
    pub input_schema: fn() -> Value,
}

const CODE_SEARCH_DESCRIPTION: &str = "Search file contents in the work tree with ripgrep. Use it for exact names, strings and patterns; read a file to see more than the matched lines. Returns path:line:text matches, or file names, or counts. Skips hidden and git-ignored files unless asked. Stops at maxResults and says so.";

/// The tools, in table order.
pub const TOOLS: &[ToolSpec] = &[ToolSpec {
    name: ToolName::CodeSearch,
    description: CODE_SEARCH_DESCRIPTION,
    input_schema: code_search_input_schema,
}];

/// The result of `tools/list`: `{"tools": [{name, description, inputSchema}]}`, keys sorted.
pub fn tools_list_result() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name.as_str(),
                "description": t.description,
                "inputSchema": (t.input_schema)(),
            })
        })
        .collect();
    canonical(&json!({ "tools": tools }))
}

// ---- code_search --------------------------------------------------------------------------------------------------------

/// The arguments of `code_search`. Strict: an unknown member is refused. Optional members are left out when absent, and the accessors give the
/// default. Parse a model's arguments with [`CodeSearchInput::from_arguments`], which answers in plain words and never repeats a value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeSearchInput {
    /// Regular expression (Rust syntax), or a literal when fixed is true.
    #[schemars(length(min = 1, max = 1000))]
    pub pattern: String,
    /// Treat pattern as a literal string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<bool>,
    /// Files or directories to search; default is the work tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10))]
    pub paths: Vec<String>,
    /// Include globs; a leading ! excludes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10))]
    pub glob: Vec<String>,
    /// Stop after this many matches; default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    pub max_results: Option<u32>,
    /// At most this many matches per file; default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 50))]
    pub max_per_file: Option<u32>,
    /// Return only the names of files that match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_only: Option<bool>,
    /// Return match counts per file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<bool>,
    /// Lines of context around each match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 5))]
    pub context: Option<u32>,
    /// Search hidden files and directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    /// Search files that .gitignore excludes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_ignored: Option<bool>,
}

/// The kinds of a member, for the check that runs before the types do.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    Flag,
    Number(u64, u64),
    TextList,
}

/// Every member of `code_search`, in the order the model sees them in the error text. `from_arguments` and the schema agree on this list
/// (`tests/tools_table.rs` drives the schema's bounds through the parser).
const MEMBERS: [(&str, Kind); 11] = [
    ("pattern", Kind::Text),
    ("fixed", Kind::Flag),
    ("paths", Kind::TextList),
    ("glob", Kind::TextList),
    ("maxResults", Kind::Number(1, MAX_RESULTS as u64)),
    ("maxPerFile", Kind::Number(1, MAX_PER_FILE as u64)),
    ("filesOnly", Kind::Flag),
    ("count", Kind::Flag),
    ("context", Kind::Number(0, MAX_CONTEXT as u64)),
    ("includeHidden", Kind::Flag),
    ("includeIgnored", Kind::Flag),
];

/// Why a call's arguments were refused. The text names the member and the rule and never repeats a value: a pattern may be a secret, and the text
/// of a refusal must be safe to show anywhere. The server puts it after `error: usage: `.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInputError {
    member: Option<&'static str>,
    rule: String,
}

impl ToolInputError {
    fn member(member: &'static str, rule: impl Into<String>) -> Self {
        Self {
            member: Some(member),
            rule: rule.into(),
        }
    }

    fn whole(rule: impl Into<String>) -> Self {
        Self {
            member: None,
            rule: rule.into(),
        }
    }

    /// The member the refusal is about, when it is about one member.
    pub fn about(&self) -> Option<&'static str> {
        self.member
    }
}

impl fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.member {
            Some(m) => write!(f, "{m}: {}", self.rule),
            None => f.write_str(&self.rule),
        }
    }
}

impl std::error::Error for ToolInputError {}

impl CodeSearchInput {
    /// The arguments of a `tools/call`: an object whose members are those of [`MEMBERS`]. `null` stands for a member left out. The check names the
    /// first member that is wrong, in the order: not an object, unknown member, missing pattern, wrong type, then the limits of [`Self::validate`].
    pub fn from_arguments(arguments: &Value) -> Result<Self, ToolInputError> {
        let Some(object) = arguments.as_object() else {
            return Err(ToolInputError::whole("arguments must be an object"));
        };
        let mut kept = Map::new();
        for (key, value) in object {
            if !MEMBERS.iter().any(|(name, _)| *name == key.as_str()) {
                let allowed: Vec<&str> = MEMBERS.iter().map(|(n, _)| *n).collect();
                return Err(ToolInputError::whole(format!(
                    "unknown member (allowed: {})",
                    allowed.join(", ")
                )));
            }
            if !value.is_null() {
                kept.insert(key.clone(), value.clone());
            }
        }
        if !kept.contains_key("pattern") {
            return Err(ToolInputError::member("pattern", "is required"));
        }
        for (name, kind) in MEMBERS {
            let Some(value) = kept.get(name) else {
                continue;
            };
            let ok = match kind {
                Kind::Text => value.is_string(),
                Kind::Flag => value.is_boolean(),
                Kind::Number(lo, hi) => value.as_u64().is_some_and(|n| (lo..=hi).contains(&n)),
                Kind::TextList => value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
            };
            if !ok {
                let rule = match kind {
                    Kind::Text => "must be text".to_string(),
                    Kind::Flag => "must be true or false".to_string(),
                    Kind::Number(lo, hi) => format!("must be a whole number from {lo} to {hi}"),
                    Kind::TextList => "must be a list of text".to_string(),
                };
                return Err(ToolInputError::member(name, rule));
            }
        }
        let input: Self = serde_json::from_value(Value::Object(kept))
            .map_err(|_| ToolInputError::whole("the arguments do not fit the tool"))?;
        input.validate()?;
        Ok(input)
    }

    /// The limits no type can state: lengths in characters and bytes, NUL bytes, and the pair that excludes each other. Also the check for an
    /// input built in code.
    pub fn validate(&self) -> Result<(), ToolInputError> {
        let chars = self.pattern.chars().count();
        if !(1..=MAX_PATTERN_CHARS).contains(&chars) {
            return Err(ToolInputError::member(
                "pattern",
                format!("must be 1 to {MAX_PATTERN_CHARS} characters"),
            ));
        }
        if self.pattern.contains('\0') {
            return Err(ToolInputError::member(
                "pattern",
                "must not hold a NUL byte",
            ));
        }
        for (name, list) in [("paths", &self.paths), ("glob", &self.glob)] {
            if list.len() > MAX_LIST_ENTRIES {
                return Err(ToolInputError::member(
                    name,
                    format!("holds at most {MAX_LIST_ENTRIES} entries"),
                ));
            }
            if list
                .iter()
                .any(|e| e.is_empty() || e.len() > MAX_ENTRY_BYTES || e.contains('\0'))
            {
                return Err(ToolInputError::member(
                    name,
                    format!("each entry is 1 to {MAX_ENTRY_BYTES} bytes without a NUL byte"),
                ));
            }
        }
        let number = |name: &'static str, v: Option<u32>, lo: u32, hi: u32| match v {
            Some(n) if !(lo..=hi).contains(&n) => Err(ToolInputError::member(
                name,
                format!("must be a whole number from {lo} to {hi}"),
            )),
            _ => Ok(()),
        };
        number("maxResults", self.max_results, 1, MAX_RESULTS)?;
        number("maxPerFile", self.max_per_file, 1, MAX_PER_FILE)?;
        number("context", self.context, 0, MAX_CONTEXT)?;
        if self.files_only() && self.count() {
            return Err(ToolInputError::whole(
                "filesOnly and count exclude each other",
            ));
        }
        Ok(())
    }

    /// `fixed`, false when absent.
    pub fn fixed(&self) -> bool {
        self.fixed.unwrap_or(false)
    }

    /// `maxResults`, [`DEFAULT_MAX_RESULTS`] when absent.
    pub fn max_results(&self) -> u32 {
        self.max_results.unwrap_or(DEFAULT_MAX_RESULTS)
    }

    /// `maxPerFile`, [`DEFAULT_MAX_PER_FILE`] when absent.
    pub fn max_per_file(&self) -> u32 {
        self.max_per_file.unwrap_or(DEFAULT_MAX_PER_FILE)
    }

    /// `filesOnly`, false when absent.
    pub fn files_only(&self) -> bool {
        self.files_only.unwrap_or(false)
    }

    /// `count`, false when absent.
    pub fn count(&self) -> bool {
        self.count.unwrap_or(false)
    }

    /// `context`, 0 when absent.
    pub fn context(&self) -> u32 {
        self.context.unwrap_or(0)
    }

    /// `includeHidden`, false when absent.
    pub fn include_hidden(&self) -> bool {
        self.include_hidden.unwrap_or(false)
    }

    /// `includeIgnored`, false when absent.
    pub fn include_ignored(&self) -> bool {
        self.include_ignored.unwrap_or(false)
    }
}

/// The `inputSchema` of `code_search` as a model reads it. It is the schemars schema of [`CodeSearchInput`] with what a model does not need taken
/// off (the dialect line, the title, the struct text, the `format` of a number) and what it does need put on: an optional member is "may be left
/// out", never "may be null" (the parser still reads `null` as left out), and the defaults the accessors apply are stated.
fn code_search_input_schema() -> Value {
    let mut root = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<CodeSearchInput>()
        .to_value();
    if let Some(obj) = root.as_object_mut() {
        for key in ["$schema", "title", "description"] {
            obj.remove(key);
        }
        if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
            for prop in props.values_mut().filter_map(Value::as_object_mut) {
                prop.remove("format");
                if let Some(Value::Array(types)) = prop.get("type") {
                    let words: Vec<&Value> = types.iter().filter(|t| *t != "null").collect();
                    if let [only] = words.as_slice() {
                        let only = (*only).clone();
                        prop.insert("type".into(), only);
                    }
                }
            }
            for (name, default) in [
                ("maxResults", DEFAULT_MAX_RESULTS),
                ("maxPerFile", DEFAULT_MAX_PER_FILE),
            ] {
                if let Some(prop) = props.get_mut(name).and_then(Value::as_object_mut) {
                    prop.insert("default".into(), json!(default));
                }
            }
        }
    }
    canonical(&root)
}

// ---- the contract document ------------------------------------------------------------------------------------------------

fn camel(word: &str) -> String {
    let mut out = String::new();
    for (i, part) in word.split('_').enumerate() {
        if i == 0 {
            out.push_str(part);
        } else {
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

/// `schemas/tools.schema.json`: a JSON Schema (2020-12) that a `tools/list` result validates against. Each tool has a descriptor definition whose
/// name and description are constants and whose `inputSchema` is the schema a model reads, so the text a model is shown is part of the frozen
/// contract and a change to it changes the file and needs a protocol version. The failure words and the server name are definitions too.
pub fn contract_schema() -> Value {
    let mut defs = Map::new();
    let mut one_of = Vec::new();
    for spec in TOOLS {
        let key = format!("{}Descriptor", camel(spec.name.as_str()));
        defs.insert(
            key.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["description", "inputSchema", "name"],
                "properties": {
                    "name": {"const": spec.name.as_str()},
                    "description": {"const": spec.description},
                    "inputSchema": (spec.input_schema)(),
                },
            }),
        );
        one_of.push(json!({ "$ref": format!("#/$defs/{key}") }));
    }
    defs.insert(
        "failureWord".into(),
        json!({
            "description": "The first word after `error: ` in the text of a failed call.",
            "type": "string",
            "enum": RESULT_FAILURE_WORDS,
        }),
    );
    defs.insert(
        "serverName".into(),
        json!({
            "description": "The MCP server name; an engine shows a tool as <server>__<tool>.",
            "const": SERVER_NAME,
        }),
    );
    canonical(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "ToolsContract",
        "description": "The result of tools/list of the harness's tool server: the tools, with the text and input schema a model is shown.",
        "type": "object",
        "additionalProperties": false,
        "required": ["tools"],
        "properties": {
            "tools": {"type": "array", "items": {"oneOf": one_of}},
        },
        "$defs": defs,
    }))
}

// ---- the note in the run summary ------------------------------------------------------------------------------------------

/// What the tool server did in one run (protocol 1.40.0): `DirectSummary.tools`, written by agentd's tools module through the `on_summary` hook.
/// Left out when the module decided nothing (a stage without the layer). Words and counts only: never a pattern, a path, a query or a result.
///
/// Every word is a plain string, validated for its form and not for membership of a list. The summary is read by components that may be older than
/// the agentd that wrote it, and an unknown value of an enum would fail the whole parse and lose the run's record.
///
/// - `enabled`: the module started the server and registered the tool list. When false, `reason` says why in one kebab word, and there are no
///   `counts`: a zero would claim that a server that never ran counted nothing.
/// - `port`: the loopback port the server bound.
/// - `connect`: what the module saw of the engine's connection 30 seconds after the first prompt of a session: `none` (nothing arrived),
///   `handshake-only` (`initialize` without `tools/list`) or `ok`. Left out when the module did not judge.
/// - `list`: the bare names of the tools the server listed, as the engine's `tools/list` read them. The stored events carry the engine's joined
///   form (`ham__code_search`); the summary never does.
/// - `counts`: present when the server ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectToolsNote {
    pub enabled: bool,
    /// Why the module is off, or why its server stopped for good: one kebab word of at most 40 characters, the writer's own set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `none`, `handshake-only` or `ok` today; a later writer may add a word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
    /// Bare tool names, at most 16, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub list: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<DirectToolsCounts>,
}

/// The counts of a run's tool server (protocol 1.40.0), part of [`DirectToolsNote`].
///
/// - `calls`: per tool, the `tools/call` requests received for a listed tool, including the ones that failed or were refused. A call that names
///   a tool the server did not list is a protocol fault and is counted nowhere, so the keys are a subset of `list`.
/// - `failed`: calls whose backend failed (`error: unavailable`, `error: timeout`, or an internal fault).
/// - `refused`: calls the server did not run (`error: usage`, `error: busy`, `error: refused`, `error: disabled`).
/// - `truncated`: calls whose answer was shortened to the result cap or to the match limits.
/// - `restarts`: times the server task ended and the module started it again on the same port (at most three).
/// - `p50Ms`, `p95Ms`: the latency of the calls that ran, in milliseconds. Left out when no call ran: a zero would claim that the server answered
///   instantly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectToolsCounts {
    #[serde(default)]
    pub calls: BTreeMap<String, u64>,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub refused: u64,
    #[serde(default)]
    pub truncated: u64,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<u64>,
}

/// A kebab word: a lowercase letter, then lowercase letters, digits and hyphens, at most `max` bytes.
fn is_kebab_word(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    (1..=max).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

impl DirectToolsNote {
    /// The rules `DirectSummary::validate` applies: the form of every word, the bounds of the lists, no repeated name, `counts` only for a module
    /// that was on, every counted tool listed, and `p50Ms` not above `p95Ms`.
    pub fn is_valid(&self) -> bool {
        if self
            .reason
            .as_deref()
            .is_some_and(|r| !is_kebab_word(r, MAX_REASON_CHARS))
            || self.port == Some(0)
            || self
                .connect
                .as_deref()
                .is_some_and(|c| !is_kebab_word(c, MAX_CONNECT_CHARS))
        {
            return false;
        }
        let listed: BTreeSet<&str> = self.list.iter().map(String::as_str).collect();
        if self.list.len() > MAX_TOOLS_LISTED
            || listed.len() != self.list.len()
            || !self.list.iter().all(|t| is_tool_word(t))
        {
            return false;
        }
        match &self.counts {
            None => true,
            Some(c) => {
                self.enabled
                    && c.calls.len() <= MAX_TOOLS_LISTED
                    && c.calls.keys().all(|k| listed.contains(k.as_str()))
                    && match (c.p50_ms, c.p95_ms) {
                        (Some(p50), Some(p95)) => p50 <= p95,
                        _ => true,
                    }
            }
        }
    }
}
