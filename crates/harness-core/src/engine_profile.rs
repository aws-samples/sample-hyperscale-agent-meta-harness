//! Engine-profile loader and validation.
//!
//! [`parse_profile`], [`validate_profile`], [`resolve_launch`] and [`hidden_paths_union`] are pure;
//! [`load_profile`] and [`load_profiles`] read files (the only I/O in this crate).
//!
//! An engine is a profile name (`harness_protocol::common::Engine`). [`BUNDLED`] is every profile
//! of this build (`build.rs` compiles `engine/profiles/*.toml` in), so the boundaries without the
//! profile files on disk (control-api, the orchestrator, the engine policy check, `hamh-dispatch`)
//! know every engine: [`known_engine`] refuses a name with no profile, and [`bundled_profile`]
//! gives its profile. A new engine is a profile file (and its fixtures) plus a rebuild.

use harness_protocol::common::{Engine, Transport, is_lower_hex};
use harness_protocol::engine_profile::{
    CancelPolicy, EngineMcp, EngineProfile, Launch, McpVia, RowClass, StdinMode,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Placeholders a profile string may use; [`resolve_launch`] substitutes them.
pub const PLACEHOLDERS: &[&str] = &[
    "workspace",
    "dataDir",
    "configDir",
    "model",
    "region",
    "systemPromptFile",
    "promptFile",
    "fixtures",
];

/// Where a profile's `[mcp]` template lives: a template path is a plain path under this directory (the same base agentd resolves it against, its
/// templates root, `/opt/harness` in the image).
pub const MCP_TEMPLATES_ROOT: &str = "engine/profiles/templates";
/// The `[mcp]` result cap: at least this many characters (protocol 1.40.0).
pub const MCP_MIN_RESULT_CHARS: u32 = 1_000;
/// The `[mcp]` result cap: at most this many characters.
pub const MCP_MAX_RESULT_CHARS: u32 = 16_000;
/// The most `[mcp] shadow` entries, and the longest one in characters.
const MCP_MAX_SHADOW: usize = 32;
const MCP_MAX_SHADOW_CHARS: usize = 64;

/// Variables that carry credentials; a profile never sets them (root invariant 2).
const CREDENTIAL_ENV: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("{path}: {message}")]
    Io { path: String, message: String },
    #[error("profile TOML: {0}")]
    Parse(String),
    #[error("schemaVersion {0} is not supported (expected 1)")]
    SchemaVersion(u32),
    #[error("profile name {0:?} must match ^[a-z][a-z0-9-]{{0,31}}$")]
    Name(String),
    #[error("profile {name} is in file {stem}.toml; the stem must equal the name")]
    NameMismatch { name: String, stem: String },
    #[error("gate name {0:?} must match ^QG-[A-Z]+$")]
    GateName(String),
    #[error("version must not be empty")]
    Version,
    #[error("transports.{0:?} argv is empty")]
    EmptyArgv(Transport),
    #[error("transports.{0:?}: ACP reads a pipe, CLI reads /dev/null")]
    Stdin(Transport),
    #[error("transports.{0:?}: cancel.killAfterS must be at least 1")]
    KillAfter(Transport),
    #[error("{field}: unknown placeholder {{{token}}}")]
    Placeholder { field: String, token: String },
    #[error("{field}: unclosed placeholder")]
    Unclosed { field: String },
    #[error("env must not set credential variable {0}")]
    CredentialEnv(String),
    #[error("env key {0:?} must match ^[A-Z_][A-Z0-9_]*$")]
    EnvKey(String),
    #[error("hidden path {0:?} must be a non-empty repo-relative path without ..")]
    HiddenPath(String),
    #[error("row id {0:?} must match ^K[0-9]{{1,2}}(-[A-Z])?$")]
    RowId(String),
    #[error("row {0} appears more than once")]
    DuplicateRow(String),
    #[error("row {0}: pass needs runs >= 1 and minPass <= runs")]
    PassRule(String),
    #[error("row {0}: a scoped row names its trigger")]
    ScopedWithoutTrigger(String),
    #[error("profile has no K rows")]
    NoRows,
    #[error("binary.sha256 {0:?} must be 64 lowercase hex characters")]
    Sha256(String),
    #[error("profile has no {0:?} transport")]
    TransportMissing(Transport),
    #[error("launch needs a value for {{{0}}}")]
    MissingVar(String),
    #[error(
        "mcp: via = \"file\" needs path, template and maxResultChars; via = \"none\" takes none of path, template, maxResultChars and shadow"
    )]
    McpFields,
    #[error(
        "mcp.path {0:?} must start with {{dataDir}}/ or {{configDir}}/ and then name the file by plain names (no placeholder, `.`, `..` or empty part)"
    )]
    McpPath(String),
    #[error(
        "mcp.template {0:?} must be a plain path under engine/profiles/templates/ (no placeholder, `.`, `..`, empty part or leading slash)"
    )]
    McpTemplate(String),
    #[error("mcp.maxResultChars {0} must be between 1000 and 16000")]
    McpMaxResultChars(u32),
    #[error(
        "mcp.shadow entry {0:?} must be 1 to 64 characters without a space, and the list holds at most 32 entries"
    )]
    McpShadow(String),
}

include!(concat!(env!("OUT_DIR"), "/bundled_profiles.rs"));

/// Why an engine name is not usable here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnknownEngine {
    #[error(transparent)]
    Name(#[from] harness_protocol::common::EngineNameError),
    #[error("engine {name:?} has no profile engine/profiles/{name}.toml (profiles: {known})")]
    NoProfile { name: String, known: String },
}

/// The engine names of [`BUNDLED`], sorted.
pub fn bundled_names() -> Vec<&'static str> {
    BUNDLED.iter().map(|(n, _)| *n).collect()
}

/// `name` as an engine that has a profile in this build.
pub fn known_engine(name: &str) -> Result<Engine, UnknownEngine> {
    let engine = Engine::new(name)?;
    if BUNDLED.iter().any(|(n, _)| *n == engine.as_str()) {
        Ok(engine)
    } else {
        Err(UnknownEngine::NoProfile {
            name: name.to_string(),
            known: bundled_names().join(", "),
        })
    }
}

/// The bundled profile of `engine`, parsed and validated; `None` when the build has none.
pub fn bundled_profile(engine: Engine) -> Option<Result<EngineProfile, ProfileError>> {
    let (stem, text) = BUNDLED.iter().find(|(n, _)| *n == engine.as_str())?;
    Some(parse_profile(text).and_then(|p| named_by(p, stem)))
}

/// The profile when its name is the file stem.
fn named_by(profile: EngineProfile, stem: &str) -> Result<EngineProfile, ProfileError> {
    if profile.name == stem {
        Ok(profile)
    } else {
        Err(ProfileError::NameMismatch {
            name: profile.name,
            stem: stem.to_string(),
        })
    }
}

/// A launch with placeholders substituted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub argv: Vec<String>,
    /// Profile `env` overlaid with the launch `env`.
    pub env: BTreeMap<String, String>,
    /// Variables to remove from the inherited environment.
    pub env_remove: Vec<String>,
    pub stdin: StdinMode,
    pub cancel: CancelPolicy,
    /// `transports.<t>.modelEnv`: the variable that pins the model at `session/new`.
    pub model_env: Option<String>,
}

/// `fixtures/engines/<name>` under the repository root: the value of `{fixtures}`.
pub fn fixtures_dir(repo_root: &Path, name: &str) -> PathBuf {
    repo_root.join("fixtures").join("engines").join(name)
}

/// Parses and validates profile TOML.
pub fn parse_profile(text: &str) -> Result<EngineProfile, ProfileError> {
    let profile: EngineProfile =
        toml::from_str(text).map_err(|e| ProfileError::Parse(e.message().to_string()))?;
    validate_profile(&profile)?;
    Ok(profile)
}

/// Reads, parses and validates one profile; the file stem must equal the profile name.
pub fn load_profile(path: &Path) -> Result<EngineProfile, ProfileError> {
    let text = std::fs::read_to_string(path).map_err(|e| ProfileError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let profile = parse_profile(&text)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    named_by(profile, stem)
}

/// Loads every `*.toml` in `dir`, sorted by name.
pub fn load_profiles(dir: &Path) -> Result<Vec<EngineProfile>, ProfileError> {
    let io = |e: std::io::Error| ProfileError::Io {
        path: dir.display().to_string(),
        message: e.to_string(),
    };
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(io)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    paths.iter().map(|p| load_profile(p)).collect()
}

/// The union of every profile's `hiddenPaths`.
pub fn hidden_paths_union(profiles: &[EngineProfile]) -> BTreeSet<String> {
    profiles
        .iter()
        .flat_map(|p| p.hidden_paths.iter().cloned())
        .collect()
}

/// Semantic checks the schema cannot express.
pub fn validate_profile(p: &EngineProfile) -> Result<(), ProfileError> {
    if p.schema_version != 1 {
        return Err(ProfileError::SchemaVersion(p.schema_version));
    }
    if !valid_name(&p.name) {
        return Err(ProfileError::Name(p.name.clone()));
    }
    if p.version.trim().is_empty() {
        return Err(ProfileError::Version);
    }
    if !valid_gate(&p.gate.name) {
        return Err(ProfileError::GateName(p.gate.name.clone()));
    }
    if let Some(sha) = &p.binary.sha256
        && !is_lower_hex(sha, 64)
    {
        return Err(ProfileError::Sha256(sha.clone()));
    }
    check_placeholders("binary.path", &p.binary.path)?;
    check_env("env", &p.env)?;
    for key in &p.env_remove {
        check_env_key(key)?;
    }
    check_launch(Transport::Acp, &p.transports.acp)?;
    if let Some(cli) = &p.transports.cli {
        check_launch(Transport::Cli, cli)?;
    }
    for (i, e) in p.config_emitters.iter().enumerate() {
        check_placeholders(&format!("configEmitters[{i}].path"), &e.path)?;
        check_placeholders(&format!("configEmitters[{i}].template"), &e.template)?;
    }
    for h in &p.hidden_paths {
        if !valid_relative(h) {
            return Err(ProfileError::HiddenPath(h.clone()));
        }
    }
    if let Some(mcp) = &p.mcp {
        check_mcp(mcp)?;
    }
    if p.rows.is_empty() {
        return Err(ProfileError::NoRows);
    }
    let mut seen = BTreeSet::new();
    for r in &p.rows {
        if !valid_row_id(&r.id) {
            return Err(ProfileError::RowId(r.id.clone()));
        }
        if !seen.insert(r.id.as_str()) {
            return Err(ProfileError::DuplicateRow(r.id.clone()));
        }
        if r.pass.runs == 0 || r.pass.min_pass > r.pass.runs {
            return Err(ProfileError::PassRule(r.id.clone()));
        }
        if r.class == RowClass::Scoped && r.trigger.is_none() {
            return Err(ProfileError::ScopedWithoutTrigger(r.id.clone()));
        }
    }
    Ok(())
}

/// Substitutes `vars` into the launch for `transport`. Every placeholder the launch uses needs a
/// value.
pub fn resolve_launch(
    p: &EngineProfile,
    transport: Transport,
    vars: &BTreeMap<&str, String>,
) -> Result<ResolvedLaunch, ProfileError> {
    let launch = match transport {
        Transport::Acp => Some(&p.transports.acp),
        Transport::Cli => p.transports.cli.as_ref(),
    }
    .ok_or(ProfileError::TransportMissing(transport))?;
    let sub = |s: &str| substitute(s, vars);
    let argv = launch
        .argv
        .iter()
        .map(|a| sub(a))
        .collect::<Result<Vec<_>, _>>()?;
    let mut env = BTreeMap::new();
    for (k, v) in p.env.iter().chain(launch.env.iter()) {
        env.insert(k.clone(), sub(v)?);
    }
    Ok(ResolvedLaunch {
        argv,
        env,
        env_remove: p.env_remove.clone(),
        stdin: launch.stdin,
        cancel: launch.cancel,
        model_env: launch.model_env.clone(),
    })
}

/// The `[mcp]` table (protocol 1.40.0). agentd renders the template as root and gives the result to the engine user, so the destination stays
/// under the engine's own directories and the template under the templates root; both are lexical checks, and the open of the file itself is
/// agentd's to do without following a link.
fn check_mcp(m: &EngineMcp) -> Result<(), ProfileError> {
    let (path, template, cap) = match m.via {
        McpVia::None => {
            return if m.path.is_some()
                || m.template.is_some()
                || m.max_result_chars.is_some()
                || !m.shadow.is_empty()
            {
                Err(ProfileError::McpFields)
            } else {
                Ok(())
            };
        }
        McpVia::File => match (&m.path, &m.template, m.max_result_chars) {
            (Some(path), Some(template), Some(cap)) => (path, template, cap),
            _ => return Err(ProfileError::McpFields),
        },
    };
    check_placeholders("mcp.path", path)?;
    if !valid_mcp_path(path) {
        return Err(ProfileError::McpPath(path.clone()));
    }
    if !valid_mcp_template(template) {
        return Err(ProfileError::McpTemplate(template.clone()));
    }
    if !(MCP_MIN_RESULT_CHARS..=MCP_MAX_RESULT_CHARS).contains(&cap) {
        return Err(ProfileError::McpMaxResultChars(cap));
    }
    for (i, entry) in m.shadow.iter().enumerate() {
        let n = entry.chars().count();
        if i >= MCP_MAX_SHADOW
            || !(1..=MCP_MAX_SHADOW_CHARS).contains(&n)
            || entry.chars().any(char::is_whitespace)
        {
            return Err(ProfileError::McpShadow(entry.clone()));
        }
    }
    Ok(())
}

/// Parts joined by `/`, each a plain name: not empty, not `.` or `..`, with no placeholder braces and no control character.
fn plain_parts(s: &str) -> bool {
    !s.is_empty()
        && s.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(['{', '}'])
                && !part.chars().any(char::is_control)
        })
}

/// `{dataDir}/` or `{configDir}/`, then plain names. Not `{workspace}` (the tree the model and git can see), not an absolute path.
fn valid_mcp_path(p: &str) -> bool {
    p.strip_prefix("{dataDir}/")
        .or_else(|| p.strip_prefix("{configDir}/"))
        .is_some_and(plain_parts)
}

/// `engine/profiles/templates/`, then plain names. A template path is never run through the placeholders.
fn valid_mcp_template(t: &str) -> bool {
    t.strip_prefix(MCP_TEMPLATES_ROOT)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(plain_parts)
}

fn check_launch(t: Transport, l: &Launch) -> Result<(), ProfileError> {
    if l.argv.is_empty() {
        return Err(ProfileError::EmptyArgv(t));
    }
    let want = match t {
        Transport::Acp => StdinMode::Pipe,
        Transport::Cli => StdinMode::Null,
    };
    if l.stdin != want {
        return Err(ProfileError::Stdin(t));
    }
    if l.cancel.kill_after_s == 0 {
        return Err(ProfileError::KillAfter(t));
    }
    let name = match t {
        Transport::Acp => "acp",
        Transport::Cli => "cli",
    };
    for a in &l.argv {
        check_placeholders(&format!("transports.{name}.argv"), a)?;
    }
    if let Some(k) = &l.model_env {
        check_env_key(k)?;
        if CREDENTIAL_ENV.contains(&k.as_str()) {
            return Err(ProfileError::CredentialEnv(k.clone()));
        }
    }
    check_env(&format!("transports.{name}.env"), &l.env)
}

fn check_env(field: &str, env: &BTreeMap<String, String>) -> Result<(), ProfileError> {
    for (k, v) in env {
        check_env_key(k)?;
        if CREDENTIAL_ENV.contains(&k.as_str()) {
            return Err(ProfileError::CredentialEnv(k.clone()));
        }
        check_placeholders(&format!("{field}.{k}"), v)?;
    }
    Ok(())
}

fn check_env_key(k: &str) -> Result<(), ProfileError> {
    let mut chars = k.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ProfileError::EnvKey(k.to_string()))
    }
}

/// The placeholder names in `s`, or the error for a malformed or unknown one.
fn tokens(field: &str, s: &str) -> Result<Vec<String>, ProfileError> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let end = after.find('}').ok_or_else(|| ProfileError::Unclosed {
            field: field.to_string(),
        })?;
        let token = &after[..end];
        if !PLACEHOLDERS.contains(&token) {
            return Err(ProfileError::Placeholder {
                field: field.to_string(),
                token: token.to_string(),
            });
        }
        out.push(token.to_string());
        rest = &after[end + 1..];
    }
    Ok(out)
}

fn check_placeholders(field: &str, s: &str) -> Result<(), ProfileError> {
    tokens(field, s).map(|_| ())
}

fn substitute(s: &str, vars: &BTreeMap<&str, String>) -> Result<String, ProfileError> {
    let mut out = s.to_string();
    for t in tokens("launch", s)? {
        let value = vars
            .get(t.as_str())
            .ok_or_else(|| ProfileError::MissingVar(t.clone()))?;
        out = out.replace(&format!("{{{t}}}"), value);
    }
    Ok(out)
}

/// The profile name rule is the engine name rule: a profile names an engine.
fn valid_name(s: &str) -> bool {
    Engine::new(s).is_ok()
}

fn valid_gate(s: &str) -> bool {
    s.strip_prefix("QG-")
        .is_some_and(|r| !r.is_empty() && r.bytes().all(|c| c.is_ascii_uppercase()))
}

fn valid_row_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('K') else {
        return false;
    };
    let (num, suffix) = match rest.split_once('-') {
        Some((n, x)) => (n, Some(x)),
        None => (rest, None),
    };
    (1..=2).contains(&num.len())
        && num.bytes().all(|c| c.is_ascii_digit())
        && suffix.is_none_or(|x| x.len() == 1 && x.bytes().all(|c| c.is_ascii_uppercase()))
}

fn valid_relative(s: &str) -> bool {
    let p = Path::new(s);
    !s.is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}
