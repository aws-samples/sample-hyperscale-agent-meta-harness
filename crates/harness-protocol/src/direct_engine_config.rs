//! The engine settings of a direct run.
//!
//! A user can keep one TOML file per engine (`~/.config/ham/engines/<engine>.toml`). `ham` reads it on the Mac, turns it into typed values and sends
//! them with the run as the optional `engineConfig` member of `StartDirectRunRequest`; agentd maps the values onto the engine's argv, one environment
//! variable and two JSON files. Comments never leave the Mac. A request that has no engine file leaves the member out and is byte for byte the
//! request of protocol 1.38.0.
//!
//! [`check_engine_config`] is the ONE check. `ham`, control-api (400 `invalid-engine-config`) and agentd (the run ends `stopped_setup_failed` before
//! a model call) all call it, so the three ends accept exactly the same documents. It is pure, takes no model and no catalog, and stops at the first
//! failure. The order is fixed and every failure has a stable [`EngineConfigProblem::code`]:
//!
//! 1. `wrong_engine`: the engine has no table in this build ([`keys_for`]);
//! 2. `bad_schema`: `schema` is not [`ENGINE_CONFIG_SCHEMA`];
//! 3. `too_large`: the serialized member is over [`MAX_ENGINE_CONFIG_BYTES`];
//! 4. `too_deep`: tables and lists nest deeper than [`MAX_ENGINE_CONFIG_DEPTH`] (the settings object is level 1);
//! 5. `too_many_keys`: more than [`MAX_ENGINE_CONFIG_LEAVES`] values (every list entry counts, an empty table or list counts as one);
//! 6. then every key, depth first, each table's keys in sorted order, and for each key: `unknown_key` or `denied_key` (an unknown key that a
//!    [`DENIED`] class names, which only chooses the words), then in its value `bad_type` (for a list, any entry), `out_of_range` (any entry outside
//!    the range or the set), `duplicate_value` and `string_too_long` (a text over [`MAX_ENGINE_CONFIG_STRING_BYTES`] bytes; a closed set refuses a long
//!    text as out of range first, so the word is for a key that would take text).
//!
//! Rules that hold in all of them. The table is an allow-list: a key that is not in it is refused. Every value is a number in a range or a member of a closed set,
//! so free text, and with it a secret, cannot ride in a value. A value is NEVER echoed in a message. A key
//! is named only when it is plain lowercase text of at most [`MAX_ENGINE_CONFIG_KEY_ECHO_BYTES`] bytes (`[a-z0-9_-]` parts joined by dots); any other
//! name is reported as "a key with unusual characters", so no escape sequence or control character reaches a terminal, a log or a card. Ranges are
//! tested with `(min..=max).contains(&x)`, so NaN fails. Nothing is clamped. An empty settings object, and an empty table of the engine
//! (`{"tools": {}}`), are accepted and set nothing. Check a member before it is serialized, hashed or forwarded: the check is bounded by the size
//! limit, the other functions are not.
//!
//! The key table ([`CLINE_KEYS`]) holds the five settings that a probe on the pinned engine binary admitted. Adding a key or widening a range keeps
//! `schema`; removing a key, narrowing a range or changing a type or a meaning bumps it. The levels of `reasoning.effort` are the single constant
//! `EFFORT_LEVELS` below: the probe package decides that list, and [`key_table_sha256`] is pinned by a test so that an edit is a conscious one.
//! Protocol 1.47.0 widened that list by `off` and `xhigh` and gave `default` the meaning "the model's catalog level"; both are additions of
//! values, so `schema` stays 1.
//!
//! Everything here is re-exported from [`crate::direct`].

use crate::common::Engine;
use crate::direct::DIRECT_ENGINE;
use crate::direct_skills::sha256_hex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::io;

/// The only schema number of the member today; it selects the key table.
pub const ENGINE_CONFIG_SCHEMA: u64 = 1;
/// The most bytes the serialized member may have.
pub const MAX_ENGINE_CONFIG_BYTES: usize = 4096;
/// The deepest nesting of tables and lists; the settings object is level 1 (`{"tools": {"disable": [...]}}` is level 3).
pub const MAX_ENGINE_CONFIG_DEPTH: usize = 3;
/// The most values in one member, every list entry counted.
pub const MAX_ENGINE_CONFIG_LEAVES: usize = 64;
/// The longest text value or list entry, in bytes.
pub const MAX_ENGINE_CONFIG_STRING_BYTES: usize = 64;
/// The longest key name (the dotted path) a message may repeat.
pub const MAX_ENGINE_CONFIG_KEY_ECHO_BYTES: usize = 40;

// ---- the member ----------------------------------------------------------------------------------------------------------

/// `StartDirectRunRequest.engineConfig` (protocol 1.39.0): the user's settings for one engine, as typed values. Strict like [`crate::direct::Skill`]:
/// a member this build does not know is refused. [`check_engine_config`] decides what `settings` may hold.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineConfig {
    /// The engine the settings are for (`cline` today); selects the key table.
    pub engine: String,
    /// The key-table version; [`ENGINE_CONFIG_SCHEMA`].
    pub schema: u64,
    /// A JSON object of tables and keys, for example `{"reasoning": {"effort": "high"}}`; the key table of the engine decides which are allowed.
    /// It is written with every object's keys in sorted order, whatever order it was read in.
    #[serde(serialize_with = "serialize_sorted")]
    pub settings: Map<String, Value>,
}

/// Writes `settings` with every object's keys in sorted order, without copying the value. (serde_json's `preserve_order` feature can be switched on
/// by any crate in a build graph, and the control-api and run-session graphs do switch it on.) It allocates one list of references per object and
/// recurses with the nesting, so call it on a member that passed [`check_engine_config`] (the wire parser stops at 128 levels).
fn serialize_sorted<S: serde::Serializer>(
    settings: &Map<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serialize_map_sorted(settings, serializer)
}

fn serialize_map_sorted<S: serde::Serializer>(
    map: &Map<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    serializer.collect_map(entries.into_iter().map(|(k, v)| (k, Sorted(v))))
}

/// A value that serializes with sorted object keys.
struct Sorted<'a>(&'a Value);

impl Serialize for Sorted<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => serialize_map_sorted(map, serializer),
            Value::Array(items) => serializer.collect_seq(items.iter().map(Sorted)),
            other => other.serialize(serializer),
        }
    }
}

/// `Debug` shows the engine, the schema and how many top-level keys there are, never a value: an unchecked member can hold any text, and a
/// `{:?}` of a request in a log line or an error path must not carry it.
impl fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineConfig")
            .field("engine", &self.engine)
            .field("schema", &self.schema)
            .field(
                "settings",
                &format_args!("<{} top-level keys; values not shown>", self.settings.len()),
            )
            .finish()
    }
}

impl EngineConfig {
    /// The SHA-256 of the member as compact JSON with every object's keys in sorted order, as lowercase hex. Two files that differ only in
    /// comments or in the order of their keys give the same value. A number counts as serde_json prints it, so `0.90` and `0.9` are one value
    /// (the file's `0.90` is read as a float before it is sent) while `600` and `600.0` are two JSON numbers (an integer key refuses the second);
    /// a build with serde_json's `arbitrary_precision` feature, which no crate of this workspace enables, would keep the spelling of a number. It
    /// is a hash of the settings, not of a file. Call it on a member that passed [`check_engine_config`].
    pub fn canonical_sha256(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }

    /// The value of the key `path` (for example `reasoning.effort`), when `path` is a key of this engine's table and the settings hold it.
    pub fn get(&self, path: &str) -> Option<&Value> {
        keys_for(&self.engine)?.iter().find(|r| r.path == path)?;
        let mut parts = path.split('.');
        let mut cur = self.settings.get(parts.next()?)?;
        for part in parts {
            cur = cur.as_object()?.get(part)?;
        }
        Some(cur)
    }

    /// How many keys of the table the settings hold (a list counts once, whatever its length).
    pub fn key_count(&self) -> u32 {
        let n = keys_for(&self.engine).map_or(0, |rows| {
            rows.iter().filter(|r| self.get(r.path).is_some()).count()
        });
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// What `summary.json` records of the settings: the engine, the schema, the hash and the count, never a value. Call it on a member that
    /// passed [`check_engine_config`] (the engine and the hash are copied as they are).
    pub fn to_ref(&self) -> DirectEngineConfigRef {
        DirectEngineConfigRef {
            engine: self.engine.clone(),
            schema: self.schema,
            sha256: self.canonical_sha256(),
            keys: self.key_count(),
        }
    }
}

/// The engine settings as `summary.json` lists them (protocol 1.39.0, `DirectSummary.engineConfig`): what the session was given, never a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectEngineConfigRef {
    pub engine: String,
    pub schema: u64,
    /// [`EngineConfig::canonical_sha256`]: 64 lowercase hex digits.
    pub sha256: String,
    /// The number of keys of the table the settings held.
    pub keys: u32,
}

impl DirectEngineConfigRef {
    /// The checks `DirectSummary::validate` applies: an engine name, a schema of 1 or more, at most [`MAX_ENGINE_CONFIG_LEAVES`] keys and a lowercase
    /// SHA-256.
    pub fn is_valid(&self) -> bool {
        self.engine.parse::<Engine>().is_ok()
            && self.schema >= 1
            && self.keys as usize <= MAX_ENGINE_CONFIG_LEAVES
            && self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

// ---- the key table -------------------------------------------------------------------------------------------------------

/// What a key holds. The check reads only this; the labels and help text of the keys live in the client (`ham-core`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyKind {
    /// One text, a member of the closed set.
    Choice(&'static [&'static str]),
    /// A whole number in `min..=max`.
    Int { min: i64, max: i64 },
    /// A finite number in `min..=max`; a whole number is read as a number.
    Float { min: f64, max: f64 },
    /// A list of distinct members of the closed set.
    Set(&'static [&'static str]),
}

impl KeyKind {
    /// Whether the range of an [`KeyKind::Int`] admits `x`.
    pub fn admits_i64(&self, x: i64) -> bool {
        match *self {
            KeyKind::Int { min, max } => (min..=max).contains(&x),
            _ => false,
        }
    }

    /// Whether the range of a [`KeyKind::Float`] admits `x`. NaN and the infinities are never in a range.
    pub fn admits_f64(&self, x: f64) -> bool {
        match *self {
            KeyKind::Float { min, max } => x.is_finite() && (min..=max).contains(&x),
            _ => false,
        }
    }

    /// The JSON type the key wants, in words.
    pub fn type_name(&self) -> &'static str {
        match self {
            KeyKind::Choice(_) => "a text",
            KeyKind::Int { .. } => "a whole number",
            KeyKind::Float { .. } => "a number",
            KeyKind::Set(_) => "a list of text",
        }
    }

    /// What the key admits, in words.
    pub fn describe(&self) -> String {
        match self {
            KeyKind::Choice(set) => format!("one of: {}", set.join(", ")),
            KeyKind::Int { min, max } => format!("a whole number from {min} to {max}"),
            KeyKind::Float { min, max } => {
                format!("a number from {} to {}", float_text(*min), float_text(*max))
            }
            KeyKind::Set(members) => {
                format!("a list of: {} (each at most once)", members.join(", "))
            }
        }
    }

    fn line(&self) -> String {
        match self {
            KeyKind::Choice(set) => format!("choice|{}", set.join(",")),
            KeyKind::Int { min, max } => format!("int|{min}..={max}"),
            KeyKind::Float { min, max } => format!("float|{min}..={max}"),
            KeyKind::Set(members) => format!("set|{}", members.join(",")),
        }
    }
}

/// `x` with at least two decimals (`0.1` reads `0.10`).
fn float_text(x: f64) -> String {
    let s = format!("{x}");
    match s.split_once('.') {
        None => format!("{s}.00"),
        Some((_, frac)) if frac.len() < 2 => format!("{s}0"),
        Some(_) => s,
    }
}

/// One key of an engine's table. `path` is the dotted name of the key in the settings object (`reasoning.effort` is
/// `{"reasoning": {"effort": ...}}`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyRow {
    pub path: &'static str,
    pub kind: KeyKind,
    /// The value the engine uses when the key is absent, as the docs say it (`600`, `agentic`, `[]`).
    pub default: &'static str,
    /// What in the harness depends on the thing this key switches off or changes. Empty for every row of the v1 table, and the table check
    /// refuses a [`KeyKind::Set`] row (a list of tools to switch off) that names a dependent: a tool that something needs is never user-switchable.
    pub required_by: &'static [&'static str],
}

impl KeyRow {
    fn line(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.path,
            self.kind.line(),
            self.default,
            self.required_by.join(",")
        )
    }
}

/// The levels of `reasoning.effort`. THE ONE TABLE to edit when the probe records say otherwise. `default` is the level the model's row of
/// `engine/catalog/models.json` gives; `off` launches the engine with no reasoning argument (GLM 5.3
/// cannot be switched off and then reasons at its own lowest level); the other four are the launch levels, [`REASONING_LEVELS`]. `none`,
/// `minimal` and `max` are never offered: the engine flag refuses the last two and `none` is the same as `off`. After an edit,
/// `key_table_sha256` changes and its pinned golden is edited with it.
const EFFORT_LEVELS: &[&str] = &["default", "off", "low", "medium", "high", "xhigh"];

/// The reasoning levels a launch can carry: the four words that follow the engine flag `--thinking` (protocol 1.47.0). The model catalog's
/// `reasoning.default`, `DirectModelSpec.defaultReasoningEffort` and `DirectSummaryEngine.reasoningEffort` hold one of them. The user's key adds
/// `default` and `off` to this list ([`CLINE_KEYS`]).
pub const REASONING_LEVELS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Where the level of a launch came from, as `DirectSummaryEngine.reasoningSource` words it: the model's catalog level, the user's own choice,
/// or the user's `off` (protocol 1.47.0).
pub const REASONING_SOURCES: &[&str] = &["default", "explicit", "off"];

/// The five keys of the Cline engine (protocol 1.39.0). A key enters this table only with a probe record on the pinned engine binary at its
/// minimum, default and maximum, and a golden of the argv, environment or file it produces (admission rule of the design).
pub const CLINE_KEYS: &[KeyRow] = &[
    KeyRow {
        path: "reasoning.effort",
        kind: KeyKind::Choice(EFFORT_LEVELS),
        default: "default",
        required_by: &[],
    },
    KeyRow {
        path: "commands.timeout_seconds",
        kind: KeyKind::Int { min: 1, max: 1800 },
        default: "600",
        required_by: &[],
    },
    KeyRow {
        path: "context.compaction",
        kind: KeyKind::Choice(&["agentic", "basic", "off"]),
        default: "agentic",
        required_by: &[],
    },
    KeyRow {
        path: "context.compaction_trigger",
        kind: KeyKind::Float {
            min: 0.10,
            max: 0.99,
        },
        default: "0.9",
        required_by: &[],
    },
    KeyRow {
        path: "tools.disable",
        kind: KeyKind::Set(&["spawn_agent", "fetch_web_content"]),
        default: "[]",
        required_by: &[],
    },
];

/// The key table of an engine, or `None` when this build has no settings for it.
pub fn keys_for(engine: &str) -> Option<&'static [KeyRow]> {
    (engine == DIRECT_ENGINE).then_some(CLINE_KEYS)
}

/// A hash of a key table: every row, its kind, range or set, default and `required_by`. A test pins it, so that an edit of the table is a conscious
/// one (it moves the worker image key and the frozen contract).
pub fn key_table_sha256(rows: &[KeyRow]) -> String {
    let mut text = String::new();
    for row in rows {
        text.push_str(&row.line());
        text.push('\n');
    }
    sha256_hex(text.as_bytes())
}

/// What is wrong with a key table itself, in words (empty for [`CLINE_KEYS`]): a path twice, a path that is both a key and a table, a default the
/// key would refuse, a table or list nested deeper than [`MAX_ENGINE_CONFIG_DEPTH`] allows, a path part outside `[a-z0-9_]`, a path the denied
/// classes would name (the allow-list is checked first, so such a row would hide a denial), and a list of tools that something depends on.
pub fn table_problems(rows: &[KeyRow]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let path = row.path;
        let parts: Vec<String> = path.split('.').map(str::to_string).collect();
        if rows.iter().take(i).any(|o| o.path == path) {
            out.push(format!("{path}: listed twice"));
        }
        if rows.iter().any(|o| {
            o.path
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('.'))
        }) {
            out.push(format!("{path}: is both a key and a table"));
        }
        if parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        }) {
            out.push(format!("{path}: a part of the path is not [a-z0-9_]+"));
        }
        let levels = parts.len() + usize::from(matches!(row.kind, KeyKind::Set(_)));
        if levels > MAX_ENGINE_CONFIG_DEPTH {
            out.push(format!(
                "{path}: needs {levels} levels; the limit is {MAX_ENGINE_CONFIG_DEPTH}"
            ));
        }
        if classify(&parts).is_some() {
            out.push(format!("{path}: a denied class names this key"));
        }
        let default_ok = match row.kind {
            KeyKind::Choice(set) => set.contains(&row.default),
            KeyKind::Int { .. } => row
                .default
                .parse::<i64>()
                .is_ok_and(|x| row.kind.admits_i64(x)),
            KeyKind::Float { .. } => row
                .default
                .parse::<f64>()
                .is_ok_and(|x| row.kind.admits_f64(x)),
            KeyKind::Set(_) => row.default == "[]",
        };
        if !default_ok {
            out.push(format!("{path}: the default is not a value of the key"));
        }
        match row.kind {
            KeyKind::Int { min, max } if min > max => {
                out.push(format!("{path}: the range is inverted"));
            }
            KeyKind::Float { min, max } if !(min.is_finite() && max.is_finite() && min <= max) => {
                out.push(format!("{path}: the range is not finite and ordered"));
            }
            KeyKind::Choice(members) | KeyKind::Set(members) => {
                let plain = |m: &&str| {
                    !m.is_empty()
                        && m.len() <= MAX_ENGINE_CONFIG_STRING_BYTES
                        && m.bytes()
                            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
                };
                let distinct = members.iter().collect::<BTreeSet<_>>().len() == members.len();
                if members.is_empty() || !members.iter().all(plain) || !distinct {
                    out.push(format!(
                        "{path}: the set is empty, repeats a member, or holds a member that is not [a-z0-9_-]{{1,64}}"
                    ));
                }
            }
            _ => {}
        }
        if matches!(row.kind, KeyKind::Set(_)) && !row.required_by.is_empty() {
            out.push(format!(
                "{path}: depended on by {}; a user may not switch it off",
                row.required_by.join(", ")
            ));
        }
    }
    out
}

// ---- the denied classes --------------------------------------------------------------------------------------------------

/// A class of keys the harness never lets a user set. The allow-list is the policy; a class only chooses the words, so that a user who types
/// `aws.profile` learns why instead of reading "unknown". `reason` is the ONE short text of the refusal and of the Settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeniedClass {
    /// A stable word (`credentials`, `provider`, `telemetry`, `processes`, `permissions`, `network`, `owned-elsewhere`).
    pub id: &'static str,
    /// The name the Settings screen lists in its "Locked by the harness" line.
    pub label: &'static str,
    /// What the class protects.
    pub protects: &'static str,
    /// One short phrase, ASCII, no quotation mark, no final period.
    pub reason: &'static str,
}

/// The seven classes, in the order of the design.
pub const DENIED: &[DeniedClass] = &[
    DeniedClass {
        id: "credentials",
        label: "credentials",
        protects: "root invariants 2 and 8",
        reason: "the harness supplies credentials itself; this cannot be set",
    },
    DeniedClass {
        id: "provider",
        label: "provider",
        protects: "root invariant 6",
        reason: "the model, provider and limits come from the run's model; use --model or default_model",
    },
    DeniedClass {
        id: "telemetry",
        label: "telemetry",
        protects: "root invariant 3",
        reason: "telemetry and updates are off for every run and stay off",
    },
    DeniedClass {
        id: "processes",
        label: "processes",
        protects: "root invariants 2 and 3",
        reason: "this would run code or move a protected folder; not available",
    },
    DeniedClass {
        id: "permissions",
        label: "approvals",
        protects: "the microVM boundary",
        reason: "the harness sets the approval mode for the run",
    },
    DeniedClass {
        id: "network",
        label: "network",
        protects: "the stage egress allow-list",
        reason: "network access is set by the stage",
    },
    DeniedClass {
        id: "owned-elsewhere",
        label: "owned elsewhere",
        protects: "one source per setting",
        reason: "set this in config.toml or with the flag",
    },
];

/// A hash of the denied table: id, label, what it protects and the reason of each class. A wording change moves the worker image key, and a test
/// pins this value so that the change is a conscious one.
pub fn denied_table_sha256() -> String {
    let mut text = String::new();
    for d in DENIED {
        text.push_str(&format!(
            "{}|{}|{}|{}\n",
            d.id, d.label, d.protects, d.reason
        ));
    }
    sha256_hex(text.as_bytes())
}

/// How a name is matched (after lowercasing and removing `_` and `-`, so `base_url`, `baseUrl` and `BASEURL` are one name).
struct Pattern {
    exact: &'static [&'static str],
    prefix: &'static [&'static str],
    contains: &'static [&'static str],
}

/// One entry per [`DENIED`] class, in the same order.
const PATTERNS: &[Pattern] = &[
    Pattern {
        exact: &[],
        prefix: &["aws"],
        contains: &[
            "key",
            "secret",
            "token",
            "password",
            "credential",
            "bearer",
            "profile",
        ],
    },
    Pattern {
        exact: &[
            "provider",
            "region",
            "baseurl",
            "endpoint",
            "contextwindow",
            "maxtokens",
        ],
        prefix: &["model"],
        contains: &[],
    },
    Pattern {
        exact: &["analytics", "otel", "feedback"],
        prefix: &["telemetry", "autoupdate"],
        contains: &[],
    },
    Pattern {
        exact: &[
            "env",
            "command",
            "args",
            "cwd",
            "path",
            "dir",
            "home",
            "hooks",
            "plugins",
            "mcpservers",
        ],
        prefix: &[],
        contains: &[],
    },
    Pattern {
        exact: &["autoapprove", "permissions", "sandbox", "shell"],
        prefix: &[],
        contains: &[],
    },
    Pattern {
        exact: &["proxy", "allowedhosts", "websearch"],
        prefix: &[],
        contains: &[],
    },
    Pattern {
        exact: &["goal", "nudge", "research", "timeout", "silence"],
        prefix: &[],
        contains: &[],
    },
];

/// The class that names one part of a path, or `None`. Exact and prefix matches of every class come before the `contains` words, so `maxTokens`
/// is a limit of the provider (its class) and not a credential that happens to contain `token`.
fn classify_part(part: &str) -> Option<&'static DeniedClass> {
    let name: String = part
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    for (class, pattern) in DENIED.iter().zip(PATTERNS) {
        if pattern.exact.contains(&name.as_str())
            || pattern.prefix.iter().any(|p| name.starts_with(p))
        {
            return Some(class);
        }
    }
    for (class, pattern) in DENIED.iter().zip(PATTERNS) {
        if pattern.contains.iter().any(|w| name.contains(w)) {
            return Some(class);
        }
    }
    None
}

/// The first class that names a part of the path.
fn classify(path: &[String]) -> Option<&'static DeniedClass> {
    path.iter().find_map(|part| classify_part(part))
}

// ---- the problem ---------------------------------------------------------------------------------------------------------

/// The first rule a member breaks. The words never contain a value. A later version may add a variant (a new rule), so a match outside this
/// crate needs a wildcard arm; [`EngineConfigProblem::code`] is the stable word.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum EngineConfigProblem {
    /// The engine has no table in this build.
    WrongEngine,
    /// `schema` is not [`ENGINE_CONFIG_SCHEMA`].
    BadSchema,
    /// The serialized member is over [`MAX_ENGINE_CONFIG_BYTES`].
    TooLarge,
    /// Tables and lists nest deeper than [`MAX_ENGINE_CONFIG_DEPTH`].
    TooDeep,
    /// More than [`MAX_ENGINE_CONFIG_LEAVES`] values.
    TooManyKeys,
    /// A key that is not in the table and that no denied class names.
    UnknownKey { key: Option<String> },
    /// A key that is not in the table and that a denied class names.
    DeniedKey {
        key: Option<String>,
        class: &'static DeniedClass,
    },
    /// The JSON type is not the key's.
    BadType {
        key: Option<String>,
        expected: &'static str,
    },
    /// The value is outside the key's range or set.
    OutOfRange {
        key: Option<String>,
        allowed: String,
    },
    /// A list names a member twice.
    DuplicateValue { key: Option<String> },
    /// A text is longer than [`MAX_ENGINE_CONFIG_STRING_BYTES`] bytes.
    StringTooLong { key: Option<String> },
}

impl EngineConfigProblem {
    /// A stable word for the problem (snake case), the same in every crate and in the shared test cases.
    pub fn code(&self) -> &'static str {
        match self {
            Self::WrongEngine => "wrong_engine",
            Self::BadSchema => "bad_schema",
            Self::TooLarge => "too_large",
            Self::TooDeep => "too_deep",
            Self::TooManyKeys => "too_many_keys",
            Self::UnknownKey { .. } => "unknown_key",
            Self::DeniedKey { .. } => "denied_key",
            Self::BadType { .. } => "bad_type",
            Self::OutOfRange { .. } => "out_of_range",
            Self::DuplicateValue { .. } => "duplicate_value",
            Self::StringTooLong { .. } => "string_too_long",
        }
    }

    /// The key the problem is about, when it is short plain text (`[a-z0-9_-]` parts joined by dots, at most 40 bytes). `None` for a problem
    /// about the whole member and for a key with unusual characters.
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::UnknownKey { key }
            | Self::DeniedKey { key, .. }
            | Self::BadType { key, .. }
            | Self::OutOfRange { key, .. }
            | Self::DuplicateValue { key }
            | Self::StringTooLong { key } => key.as_deref(),
            _ => None,
        }
    }

    /// The class of a denied key.
    pub fn denied_class(&self) -> Option<&'static DeniedClass> {
        match self {
            Self::DeniedKey { class, .. } => Some(class),
            _ => None,
        }
    }

    /// Whether the problem is about one key (and so has a subject in the message).
    fn is_keyed(&self) -> bool {
        !matches!(
            self,
            Self::WrongEngine
                | Self::BadSchema
                | Self::TooLarge
                | Self::TooDeep
                | Self::TooManyKeys
        )
    }

    /// The rule that was broken, in one phrase: the short reason of the class for a denied key, what the key admits for a type or range problem.
    pub fn rule(&self) -> String {
        match self {
            Self::WrongEngine => "this build has no settings for that engine".to_string(),
            Self::BadSchema => format!("schema must be {ENGINE_CONFIG_SCHEMA}"),
            Self::TooLarge => {
                format!("the settings are larger than {MAX_ENGINE_CONFIG_BYTES} bytes")
            }
            Self::TooDeep => {
                format!("the settings nest deeper than {MAX_ENGINE_CONFIG_DEPTH} levels")
            }
            Self::TooManyKeys => {
                format!("the settings hold more than {MAX_ENGINE_CONFIG_LEAVES} values")
            }
            Self::UnknownKey { .. } => "not an engine setting".to_string(),
            Self::DeniedKey { class, .. } => class.reason.to_string(),
            Self::BadType { expected, .. } => format!("must be {expected}"),
            Self::OutOfRange { allowed, .. } => format!("must be {allowed}"),
            Self::DuplicateValue { .. } => "lists a value twice".to_string(),
            Self::StringTooLong { .. } => {
                format!("holds a text longer than {MAX_ENGINE_CONFIG_STRING_BYTES} bytes")
            }
        }
    }
}

impl fmt::Display for EngineConfigProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.is_keyed() {
            return write!(f, "engine config: {}", self.rule());
        }
        let subject = match self.key() {
            Some(k) => format!("key \"{k}\""),
            None => "a key with unusual characters".to_string(),
        };
        match self {
            Self::UnknownKey { .. } => {
                write!(f, "engine config: {subject} is not an engine setting")
            }
            Self::DeniedKey { class, .. } => {
                write!(
                    f,
                    "engine config: {subject} is not allowed: {}",
                    class.reason
                )
            }
            _ => write!(f, "engine config: {subject} {}", self.rule()),
        }
    }
}

impl std::error::Error for EngineConfigProblem {}

// ---- the check -----------------------------------------------------------------------------------------------------------

/// A writer that only counts, and fails once the running total passes `limit`.
struct Counter<'a> {
    used: &'a mut usize,
    limit: usize,
}

impl io::Write for Counter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        *self.used = self.used.saturating_add(buf.len());
        if *self.used > self.limit {
            return Err(io::Error::other("over the limit"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Adds `n` to the running total; false once it passes `limit`.
fn spend(used: &mut usize, n: usize, limit: usize) -> bool {
    *used = used.saturating_add(n);
    *used <= limit
}

/// Adds the compact JSON length of a string (quotes and escapes as the serializer writes them), stopping as soon as the total passes `limit`,
/// so a huge string costs no more than the limit.
fn spend_string(s: &str, used: &mut usize, limit: usize) -> bool {
    // A string's JSON is at least its bytes and two quotes: a string longer than what is left is over the limit without being read.
    if s.len().saturating_add(2) > limit.saturating_sub(*used) {
        *used = limit.saturating_add(1);
        return false;
    }
    serde_json::to_writer(Counter { used, limit }, s).is_ok()
}

/// Adds the length of a value that has no children; false once the total passes `limit`.
fn spend_scalar(v: &Value, used: &mut usize, limit: usize) -> bool {
    match v {
        Value::Null => spend(used, 4, limit),
        Value::Bool(b) => spend(used, if *b { 4 } else { 5 }, limit),
        Value::Number(n) => serde_json::to_writer(Counter { used, limit }, n).is_ok(),
        Value::String(s) => spend_string(s, used, limit),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// One container that is being counted.
enum Frame<'a> {
    Object(serde_json::map::Iter<'a>, bool),
    Array(std::slice::Iter<'a, Value>, bool),
}

/// Whether the compact JSON of `c` is longer than `limit` bytes. It counts as the serializer writes, with no recursion (an explicit stack that
/// grows with the nesting actually read, at least one byte per level, so about 4,000 frames at most) and no allocation proportional to the input, and it stops at the first
/// byte over the limit. It is called after the engine and the schema were checked, so the head of the member is a few bytes of known text.
fn serialized_len_exceeds(c: &EngineConfig, limit: usize) -> bool {
    // The head is what the serializer writes for the same member with no settings, `<head>{}}`; the settings object's own closing brace is spent
    // when its frame closes below, so one byte less is spent here.
    let empty = EngineConfig {
        engine: c.engine.clone(),
        schema: c.schema,
        settings: Map::new(),
    };
    let base = serde_json::to_vec(&empty).map_or(usize::MAX, |v| v.len());
    let mut used = 0usize;
    if !spend(&mut used, base.saturating_sub(1), limit) {
        return true;
    }
    let mut stack = vec![Frame::Object(c.settings.iter(), true)];
    while let Some(top) = stack.last_mut() {
        // Close the container, or take its next entry: (comma before it, key, value).
        let (comma, key, value) = match top {
            Frame::Object(it, first) => match it.next() {
                None => {
                    stack.pop();
                    if !spend(&mut used, 1, limit) {
                        return true;
                    }
                    continue;
                }
                Some((k, v)) => (!std::mem::replace(first, false), Some(k.as_str()), v),
            },
            Frame::Array(it, first) => match it.next() {
                None => {
                    stack.pop();
                    if !spend(&mut used, 1, limit) {
                        return true;
                    }
                    continue;
                }
                Some(v) => (!std::mem::replace(first, false), None, v),
            },
        };
        if comma && !spend(&mut used, 1, limit) {
            return true;
        }
        if let Some(k) = key
            && !(spend_string(k, &mut used, limit) && spend(&mut used, 1, limit))
        {
            return true;
        }
        match value {
            Value::Object(m) => {
                if !spend(&mut used, 1, limit) {
                    return true;
                }
                stack.push(Frame::Object(m.iter(), true));
            }
            Value::Array(a) => {
                if !spend(&mut used, 1, limit) {
                    return true;
                }
                stack.push(Frame::Array(a.iter(), true));
            }
            scalar => {
                if !spend_scalar(scalar, &mut used, limit) {
                    return true;
                }
            }
        }
    }
    false
}

fn map_within(m: &Map<String, Value>, level: usize, limit: usize) -> bool {
    level <= limit && m.values().all(|v| value_within(v, level + 1, limit))
}

fn value_within(v: &Value, level: usize, limit: usize) -> bool {
    match v {
        Value::Object(m) => map_within(m, level, limit),
        Value::Array(a) => level <= limit && a.iter().all(|x| value_within(x, level + 1, limit)),
        _ => true,
    }
}

fn leaves(v: &Value) -> usize {
    match v {
        Value::Object(m) if !m.is_empty() => {
            m.values().fold(0usize, |n, x| n.saturating_add(leaves(x)))
        }
        Value::Array(a) if !a.is_empty() => {
            a.iter().fold(0usize, |n, x| n.saturating_add(leaves(x)))
        }
        _ => 1,
    }
}

/// The dotted name of a path when it is short plain text, else `None`.
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
    (joined.len() <= MAX_ENGINE_CONFIG_KEY_ECHO_BYTES).then_some(joined)
}

fn row_at<'t>(rows: &'t [KeyRow], path: &[String]) -> Option<&'t KeyRow> {
    rows.iter()
        .find(|r| r.path.split('.').eq(path.iter().map(String::as_str)))
}

/// Whether `path` is a table that holds keys of the engine (a strict prefix of some row's path).
fn is_table(rows: &[KeyRow], path: &[String]) -> bool {
    rows.iter().any(|r| {
        let mut parts = r.path.split('.');
        path.iter().all(|p| parts.next() == Some(p.as_str())) && parts.next().is_some()
    })
}

/// A key the table does not hold. The path is followed down the first key of each table below it, so the problem names the whole dotted path
/// (`aws.profile`), and every part of it is matched against the denied classes.
fn refuse_unknown(path: &mut Vec<String>, value: &Value) -> EngineConfigProblem {
    let mut cur = value;
    while let Value::Object(m) = cur {
        let Some(first) = m.keys().min() else { break };
        path.push(first.clone());
        let Some(next) = m.get(first) else { break };
        cur = next;
    }
    let key = safe_key(path);
    match classify(path) {
        Some(class) => EngineConfigProblem::DeniedKey { key, class },
        None => EngineConfigProblem::UnknownKey { key },
    }
}

/// The value of one key. Within a value the problems rank as in the module documentation: the JSON type (every list entry), then the range or
/// the set (every entry), then a repeated entry, then the length of a text.
fn check_value(row: &KeyRow, path: &[String], value: &Value) -> Result<(), EngineConfigProblem> {
    let key = safe_key(path);
    let bad_type = |key: Option<String>| EngineConfigProblem::BadType {
        key,
        expected: row.kind.type_name(),
    };
    let out_of_range = |key: Option<String>| EngineConfigProblem::OutOfRange {
        key,
        allowed: row.kind.describe(),
    };
    let too_long = |s: &str| s.len() > MAX_ENGINE_CONFIG_STRING_BYTES;
    match row.kind {
        KeyKind::Choice(set) => {
            let Value::String(s) = value else {
                return Err(bad_type(key));
            };
            if !set.contains(&s.as_str()) {
                return Err(out_of_range(key));
            }
            if too_long(s) {
                return Err(EngineConfigProblem::StringTooLong { key });
            }
            Ok(())
        }
        KeyKind::Int { .. } => {
            let Value::Number(n) = value else {
                return Err(bad_type(key));
            };
            // A float, or an integer above i64::MAX, is not a value of this key at all.
            let Some(i) = n.as_i64() else {
                return Err(bad_type(key));
            };
            if row.kind.admits_i64(i) {
                Ok(())
            } else {
                Err(out_of_range(key))
            }
        }
        KeyKind::Float { .. } => {
            let Value::Number(n) = value else {
                return Err(bad_type(key));
            };
            match n.as_f64() {
                Some(x) if row.kind.admits_f64(x) => Ok(()),
                Some(_) => Err(out_of_range(key)),
                None => Err(bad_type(key)),
            }
        }
        KeyKind::Set(members) => {
            let Value::Array(items) = value else {
                return Err(bad_type(key));
            };
            let mut texts: Vec<&str> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => texts.push(s),
                    _ => return Err(bad_type(key)),
                }
            }
            if texts.iter().any(|t| !members.contains(t)) {
                return Err(out_of_range(key));
            }
            let mut seen = BTreeSet::new();
            if !texts.iter().all(|t| seen.insert(*t)) {
                return Err(EngineConfigProblem::DuplicateValue { key });
            }
            if texts.iter().any(|t| too_long(t)) {
                return Err(EngineConfigProblem::StringTooLong { key });
            }
            Ok(())
        }
    }
}

/// Visits the tables depth first, each table's keys in sorted order (a table before the keys after it, component by component).
fn walk(
    rows: &[KeyRow],
    map: &Map<String, Value>,
    path: &mut Vec<String>,
) -> Result<(), EngineConfigProblem> {
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    for name in names {
        let Some(value) = map.get(name) else { continue };
        path.push(name.clone());
        let result = if let Some(row) = row_at(rows, path) {
            check_value(row, path, value)
        } else if is_table(rows, path) {
            match value {
                Value::Object(inner) => walk(rows, inner, path),
                _ => Err(EngineConfigProblem::BadType {
                    key: safe_key(path),
                    expected: "a table",
                }),
            }
        } else {
            Err(refuse_unknown(path, value))
        };
        path.pop();
        result?;
    }
    Ok(())
}

/// The ONE check of an engine's settings. See the module documentation for the order and the rules. `ham`, control-api and agentd call it.
pub fn check_engine_config(c: &EngineConfig) -> Result<(), EngineConfigProblem> {
    let Some(rows) = keys_for(&c.engine) else {
        return Err(EngineConfigProblem::WrongEngine);
    };
    check_rows(rows, c)
}

/// [`check_engine_config`] over a given table (the engine was already found).
fn check_rows(rows: &[KeyRow], c: &EngineConfig) -> Result<(), EngineConfigProblem> {
    if c.schema != ENGINE_CONFIG_SCHEMA {
        return Err(EngineConfigProblem::BadSchema);
    }
    if serialized_len_exceeds(c, MAX_ENGINE_CONFIG_BYTES) {
        return Err(EngineConfigProblem::TooLarge);
    }
    // Reached only by a member of at most 4096 bytes (so at most about 4,000 levels); the walk stops at the first level over the limit.
    if !map_within(&c.settings, 1, MAX_ENGINE_CONFIG_DEPTH) {
        return Err(EngineConfigProblem::TooDeep);
    }
    let count = if c.settings.is_empty() {
        0
    } else {
        c.settings
            .values()
            .fold(0usize, |n, v| n.saturating_add(leaves(v)))
    };
    if count > MAX_ENGINE_CONFIG_LEAVES {
        return Err(EngineConfigProblem::TooManyKeys);
    }
    walk(rows, &c.settings, &mut Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn cfg(settings: Value) -> EngineConfig {
        EngineConfig {
            engine: "cline".into(),
            schema: 1,
            settings: settings.as_object().cloned().unwrap_or_default(),
        }
    }

    /// A table with a text key whose one member is longer than the limit, so the last rule of a value can be reached.
    const LONG_MEMBER: &str =
        "a-member-of-a-closed-set-that-is-longer-than-the-sixty-four-byte-limit-of-a-text";
    const SYNTHETIC: &[KeyRow] = &[
        KeyRow {
            path: "mode.long",
            kind: KeyKind::Choice(&[LONG_MEMBER]),
            default: LONG_MEMBER,
            required_by: &[],
        },
        KeyRow {
            path: "mode.many",
            kind: KeyKind::Set(&[LONG_MEMBER, "b"]),
            default: "[]",
            required_by: &[],
        },
    ];

    #[test]
    fn a_text_over_64_bytes_is_string_too_long_only_after_the_range_and_the_set_rules_pass() {
        assert!(LONG_MEMBER.len() > MAX_ENGINE_CONFIG_STRING_BYTES);
        for settings in [
            json!({"mode": {"long": LONG_MEMBER}}),
            json!({"mode": {"many": [LONG_MEMBER, "b"]}}),
        ] {
            let p = check_rows(SYNTHETIC, &cfg(settings)).unwrap_err();
            assert_eq!(p.code(), "string_too_long");
            assert!(p.key().is_some_and(|k| k.starts_with("mode.")), "{p}");
            assert!(!p.to_string().contains(LONG_MEMBER), "{p}");
        }
        // Earlier ranks win over the length: a long text that is not a member is out of range, a repeat is a repeat.
        let long_not_member = "x".repeat(MAX_ENGINE_CONFIG_STRING_BYTES + 1);
        assert_eq!(
            check_rows(SYNTHETIC, &cfg(json!({"mode": {"long": long_not_member}})))
                .unwrap_err()
                .code(),
            "out_of_range"
        );
        assert_eq!(
            check_rows(
                SYNTHETIC,
                &cfg(json!({"mode": {"many": [LONG_MEMBER, LONG_MEMBER]}}))
            )
            .unwrap_err()
            .code(),
            "duplicate_value"
        );
    }

    #[test]
    fn the_head_of_the_member_is_what_the_serializer_writes() {
        // `serialized_len_exceeds` derives its head from the serializer; this pins the text it stands for, so a renamed or added field shows here.
        assert_eq!(
            serde_json::to_string(&cfg(json!({}))).unwrap(),
            r#"{"engine":"cline","schema":1,"settings":{}}"#
        );
        let empty = cfg(json!({}));
        assert!(!serialized_len_exceeds(&empty, 43));
        assert!(serialized_len_exceeds(&empty, 42));
        // an engine that needs escaping is counted as written
        let mut odd = cfg(json!({}));
        odd.engine = "a\"b".into();
        let len = serde_json::to_vec(&odd).unwrap().len();
        assert!(!serialized_len_exceeds(&odd, len));
        assert!(serialized_len_exceeds(&odd, len - 1));
    }

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            any::<u64>().prop_map(|n| json!(n)),
            any::<f64>().prop_filter_map("finite", |f| serde_json::Number::from_f64(f)
                .map(Value::Number)),
            ".{0,40}".prop_map(Value::String),
            "[\\u{0}-\\u{1f}\"\\\\é€😀]{0,12}".prop_map(Value::String),
        ];
        leaf.prop_recursive(5, 80, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map(".{0,10}", inner, 0..6)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, ..ProptestConfig::default() })]

        /// The running count is the length the serializer writes, and the limit test agrees with it on both sides of the boundary.
        #[test]
        fn the_streaming_size_is_the_length_of_the_serialized_member(
            settings in prop::collection::btree_map(".{0,10}", json_value(), 0..6)
        ) {
            let c = EngineConfig { engine: "cline".into(), schema: 1, settings: settings.into_iter().collect() };
            let len = serde_json::to_vec(&c).unwrap().len();
            prop_assert!(!serialized_len_exceeds(&c, len), "{len} bytes are within {len}");
            prop_assert!(serialized_len_exceeds(&c, len - 1), "{len} bytes are over {}", len - 1);
            prop_assert!(!serialized_len_exceeds(&c, len + 1));
        }
    }

    #[test]
    fn a_huge_value_is_refused_without_being_copied_or_walked() {
        // 64 MiB of text: the count stops at the 4,097th byte, and the sorted writer is never reached.
        let c = cfg(json!({"zz": "x".repeat(64 * 1024 * 1024)}));
        let started = std::time::Instant::now();
        assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_large");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // A shallow map with a million entries stops after about 4,096 bytes of them.
        let many: Map<String, Value> = (0..1_000_000)
            .map(|i| (format!("k{i}"), json!(1)))
            .collect();
        let c = EngineConfig {
            engine: "cline".into(),
            schema: 1,
            settings: many,
        };
        let started = std::time::Instant::now();
        assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_large");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_hand_made_value_nested_far_deeper_than_the_wire_allows_is_too_large_and_not_a_stack_overflow()
     {
        // 100,000 levels of lists built without recursion; the count stops after about 4,000 of them. (Dropping such a value is the caller's hazard.)
        let mut v = json!(1);
        for _ in 0..100_000 {
            v = Value::Array(vec![v]);
        }
        let mut c = cfg(json!({}));
        c.settings.insert("zz".into(), v);
        assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_large");
        // Leak the deep value: its recursive destructor is a hazard of the test and not of the check.
        std::mem::forget(c);
    }
}
