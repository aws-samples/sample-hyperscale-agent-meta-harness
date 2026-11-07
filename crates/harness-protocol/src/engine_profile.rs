//! One file per engine, `engine/profiles/<name>.toml`; `harness-core::engine_profile` loads and
//! validates it, and `schemas/engine-profile.schema.json` is exported from these types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineProfile {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    /// Engine name; equals the profile file stem.
    #[schemars(pattern(r"^[a-z][a-z0-9-]{0,31}$"))]
    pub name: String,
    /// Version the engine reports as ACP `initialize` `agentInfo.version`.
    #[schemars(length(min = 1))]
    pub version: String,
    pub binary: EngineBinary,
    pub transports: EngineTransports,
    /// Environment for every launch; values may use placeholders.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables removed from the inherited environment (for example `AWS_BEARER_TOKEN_BEDROCK`).
    #[serde(default)]
    pub env_remove: Vec<String>,
    /// Engine config files `agentd` renders per session.
    #[serde(default)]
    pub config_emitters: Vec<ConfigEmitter>,
    /// Repo-relative paths `agentd` hides from every engine.
    #[serde(default)]
    pub hidden_paths: Vec<String>,
    pub gate: QualificationGate,
    pub rows: Vec<ProfileRow>,
    /// How the engine learns of the harness's tool server (protocol 1.40.0). Left out of a profile that has no table, which gets no tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<EngineMcp>,
}

/// The `[mcp]` table of an engine profile (protocol 1.40.0, wire B of the tool layer): how the engine learns of the harness's tool server.
///
/// `via = "file"` names a settings file the engine reads its tool servers from. agentd renders `template` once it has bound the server's port and
/// writes the result to `path`; both are checked by `harness-core` (`path` under the engine's own `{dataDir}` or `{configDir}`, `template` under
/// the templates root). `via = "none"` says the engine takes no server, and the table holds nothing else. A profile with no table is an engine the
/// layer has not been wired to; the two behave the same, and `none` records that the choice was made.
///
/// `maxResultChars` caps the serialized result of a call, from what the engine does to a long result (Cline cuts it at 8,000 characters). `shadow`
/// names engine built-in tools to switch off once their use is counted; it is empty today.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineMcp {
    pub via: McpVia,
    /// Where the engine reads its tool servers from; may use `{dataDir}` and `{configDir}`. Required for `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Repo-relative path of the registration template, under `engine/profiles/templates/`. Required for `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The most characters of a serialized tool result, 1,000 to 16,000. Required for `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1000, max = 16000))]
    pub max_result_chars: Option<u32>,
    /// Engine built-in tools to disable once their use is counted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadow: Vec<String>,
}

/// How an engine is told about the tool server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpVia {
    /// A settings file the engine reads.
    File,
    /// The engine takes no server.
    None,
}

/// What `via = "file"` has, all of it present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpRegistration<'a> {
    pub path: &'a str,
    pub template: &'a str,
    pub max_result_chars: u32,
    pub shadow: &'a [String],
}

impl EngineMcp {
    /// The file registration, or `None` for `via = "none"` and for a file table that lacks a part (which `harness-core` refuses).
    pub fn registration(&self) -> Option<McpRegistration<'_>> {
        if self.via != McpVia::File {
            return None;
        }
        Some(McpRegistration {
            path: self.path.as_deref()?,
            template: self.template.as_deref()?,
            max_result_chars: self.max_result_chars?,
            shadow: &self.shadow,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineBinary {
    /// Executable name or path; may use placeholders.
    pub path: String,
    /// SHA-256 of the promoted binary, when the profile pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(pattern(r"^[0-9a-f]{64}$"))]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineTransports {
    /// Transport A, the ACP seam; every engine has it.
    pub acp: Launch,
    /// Transport B (the Cline CLI per turn), when the engine has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<Launch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Launch {
    #[schemars(length(min = 1))]
    pub argv: Vec<String>,
    /// Launch-specific environment, applied over the profile `env`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub stdin: StdinMode,
    pub cancel: CancelPolicy,
    /// The variable the engine reads its model from at `session/new` (Cline fork patch 0005:
    /// `CLINE_MODEL`); set only for `ModelPin::SessionNew` turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_env: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StdinMode {
    /// JSON-RPC over stdio (ACP).
    Pipe,
    /// `/dev/null`.
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelPolicy {
    pub method: CancelMethod,
    /// SIGKILL this many seconds after the cancel if the engine is still running.
    #[schemars(range(min = 1))]
    pub kill_after_s: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CancelMethod {
    /// ACP `session/cancel`.
    AcpCancel,
    /// SIGTERM to the process group.
    Sigterm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigEmitter {
    /// Output path; may use placeholders (normally under `{configDir}` or `{dataDir}`).
    pub path: String,
    /// Repo-relative template path.
    pub template: String,
    pub format: ConfigFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    Json,
    Toml,
    Yaml,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QualificationGate {
    #[schemars(pattern(r"^QG-[A-Z]+$"))]
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileRow {
    #[schemars(pattern(r"^K[0-9]{1,2}(-[A-Z])?$"))]
    pub id: String,
    pub class: RowClass,
    /// Trigger that fires when the row fails; required for scoped rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(default)]
    pub local: bool,
    /// What the row checks, in one line.
    pub check: String,
    pub pass: PassRule,
}

/// Gating rows must pass for the engine to hold any route; scoped rows restrict only
/// the routes their trigger names; outside rows gate something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RowClass {
    Gating,
    Scoped,
    Outside,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PassRule {
    /// Passing runs needed out of `runs`.
    pub min_pass: u32,
    #[schemars(range(min = 1))]
    pub runs: u32,
    /// Named numeric pass values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thresholds: BTreeMap<String, f64>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum Trigger {
    T1,
    T2,
    T3,
    T4,
    T5,
    T6,
    T7,
    #[serde(rename = "T8-promotion")]
    T8Promotion,
    #[serde(rename = "T8-terminal")]
    T8Terminal,
    T9,
    T10,
}
