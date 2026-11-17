//! The OpenCode arm of agentd: the session configuration written before the engine starts, the value of its ACP `model`
//! option, what the launch adds once the modules ran, and the engine's defaults for the watchdog. Everything OpenCode-specific that the
//! engine-neutral layers do not already cover lives here.
//!
//! What the configuration does, key by key (verified in the source of 1.18.32 and 2.2.6):
//!
//! - `provider.amazon-bedrock.options.profile = "agent"`: the engine builds the AWS SDK's default credential chain for that profile, whose
//!   `credential_process` is `agentd creds`. No token, key or bearer variable is written or passed (root invariant 2).
//! - `share = "disabled"`, `autoupdate = false` (v2: `update = "disable"`), `snapshot = false`, `lsp = false`, `enabled_providers`: the
//!   engine talks to Bedrock and to nothing else (root invariant 3).
//! - `permission`: no rule is `ask`. An unanswered ask hangs a turn, and the ACP layer of v1 never forwards the ask of a child session to the
//!   client at all. `doom_loop` is `deny`: the third identical call is refused by a rule, the model reads the error and changes course (a rule
//!   denial does not end the loop; only a user's rejection does). `external_directory` is `allow`: the operating system's sandbox, not the
//!   engine's own ask, is the boundary of the tree. `question`, `plan_enter` and `plan_exit` are `deny`: they are the tools that wait for a person.
//! - `instructions` and `skills.paths` name the placed rules and skills, which the engine-neutral layer puts in a folder the engine cannot write.
//! - `plugins`: one entry, the folder `plugins/hamh-shell-limit` under the harness's root by its absolute path, with
//!   `options.timeoutMs`, the run's command time limit. The pinned engine reads no variable and no configuration key for the limit of its shell
//!   tool (120 s for a call that names none); the plugin ([`SHELL_LIMIT_PLUGIN`]) gives such a call the run's limit and changes nothing else. An
//!   absolute folder is loaded as it is: nothing is installed and no registry is asked.
//!
//! Two variables belong with the plugin: `OPENCODE_PRINT_LOGS=1` and `OPENCODE_LOG_LEVEL=WARN`. With them the engine's warnings and errors, and
//! the one line the plugin writes when its hook is in place ([`READY_PREFIX`]), reach the engine's error log, where agentd looks for the line
//! after `session/new`. The level also applies to the engine's own log file, which then holds warnings and errors only; nothing of the harness
//! reads that file.
//!
//! The document is written once, into the root the harness owns (`engine::layout`, the separate root of this engine: root-owned, read-only to
//! the engine), named by `OPENCODE_CONFIG`, and handed over a second time in `OPENCODE_CONFIG_CONTENT`. The inline layer outranks every file the
//! engine could write (a repository's `opencode.json`, `~/.opencode/opencode.json`, the writable global folder), so a file a model plants cannot
//! change the policy at the next start. The two are the same document by construction; a release that stopped honouring either would show at once
//! as a model that is not the one the run asked for (the pin check of the driver fails the run before the first prompt).

use crate::emit::{EmitContext, Emitted, pretty, write};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub const ENGINE: &str = "opencode";
/// The provider the engine reaches Bedrock through.
const PROVIDER: &str = "amazon-bedrock";
/// The engine's own cap on output tokens when a model declares none: the `limit.output` written for a turn with no cap of its own.
const DEFAULT_OUTPUT_TOKENS: u32 = 32_000;
/// The inline layer of the configuration.
const CONFIG_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
/// The tool server's address, set by the tools module once the server answers (`HAMH_` prefix; `compat::set_both` sets the earlier one too).
const TOOLS_URL: &str = "HAMH_TOOLS_URL";
/// How long a tool call may run with the engine saying nothing at all before agentd gives up on it, in seconds (see `engine::watchdog`).
/// The ACP layer of v1 shows nothing of a child session, so a helper agent that works for a long time is silent here too.
pub const TOOL_IDLE_S: u64 = 3_600;

/// The text of the plugin that gives a shell call that names no time limit the run's limit. Compiled in and written per session beside
/// the session document, so the text and agentd cannot differ; the limit travels in the plugin's options.
pub const SHELL_LIMIT_PLUGIN: &str = include_str!("opencode_shell_limit.js");
/// The plugin's own id.
pub const SHELL_LIMIT_ID: &str = "hamh.shell-limit";
/// The plugin's folder, relative to the harness's root.
pub const SHELL_LIMIT_DIR: &str = "plugins/hamh-shell-limit";
/// How the line starts that the plugin writes to the engine's error log when its hook is in place; the limit in milliseconds follows.
pub const READY_PREFIX: &str = "hamh.shell-limit: ready ";
/// Makes the engine's server process write its log to standard error (it is discarded otherwise).
const PRINT_LOGS: (&str, &str) = ("OPENCODE_PRINT_LOGS", "1");
/// Warnings and errors only: the reason a plugin did not load is a warning.
const LOG_LEVEL: (&str, &str) = ("OPENCODE_LOG_LEVEL", "WARN");
/// The pinned engine's own limit for a shell call that names none, in milliseconds.
pub const ENGINE_DEFAULT_LIMIT_MS: u64 = 120_000;
/// The one sentence of a read-back that fails: it names the part and never a value or a path.
const LIMIT_NOT_HELD: &str =
    "engine settings: the engine's configuration does not hold the command time limit";

/// The plugin's folder under the harness's root `root`.
pub fn shell_limit_dir(root: &Path) -> PathBuf {
    root.join(SHELL_LIMIT_DIR)
}

/// The two release lines of the engine. They read one document, but name some switches differently and lack some of each other's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    V1,
    V2,
}

impl Line {
    /// The line of a profile's `version`. A version that cannot be read is the line the pin had when this was written.
    pub fn of_version(version: &str) -> Line {
        let major = version
            .trim()
            .split(['.', '-', '+'])
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        match major {
            Some(n) if n >= 2 => Line::V2,
            _ => Line::V1,
        }
    }
}

/// The `provider.amazon-bedrock` member: Region, profile and the one route of the run. With the models fetch off the engine knows only the
/// catalog it was built with, which lacks the newest models, so the route is declared here with the model's own limits.
fn provider(ctx: &EmitContext) -> Value {
    let id = ctx.model.as_str();
    let mut entry = json!({ "name": id, "tool_call": true });
    if let Some(context) = ctx.window() {
        entry["limit"] = json!({
            "context": context,
            "output": ctx.max_output_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS),
        });
    }
    let mut models = Map::new();
    models.insert(id.to_string(), entry);
    json!({
        "options": { "region": ctx.region, "profile": "agent" },
        "models": models,
    })
}

/// The `permission` member of a line.
fn permission(line: Line) -> Value {
    let mut p = json!({
        "external_directory": "allow",
        "read": "allow",
        "question": "deny",
        "plan_enter": "deny",
        "plan_exit": "deny",
        "websearch": "deny",
    });
    // v2 has no `doom_loop` action.
    if line == Line::V1 {
        p["doom_loop"] = json!("deny");
    }
    p
}

/// The engine's own compaction keys for the user's setting: the switch, and the buffer that makes it start at the user's share of the window.
fn compaction(ctx: &EmitContext) -> Option<Value> {
    let block = ctx.compaction.as_ref()?;
    let mut c = Map::new();
    if let Some(enabled) = block.enabled {
        c.insert("auto".into(), json!(enabled));
    }
    if let (Some(ratio), Some(window)) = (block.threshold_ratio, ctx.window()) {
        c.insert(
            "reserved".into(),
            json!(reserved_tokens(ratio.get(), window)),
        );
    }
    (!c.is_empty()).then_some(Value::Object(c))
}

/// The tokens to keep free so that compaction starts when `ratio` of `window` is used.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn reserved_tokens(ratio: f64, window: u32) -> u32 {
    ((1.0 - ratio) * f64::from(window))
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

/// The session document of a line. Pure: the model value and the rules and skills folders come from the engine-neutral layer.
pub fn document(ctx: &EmitContext, line: Line) -> Value {
    let model = super::config::acp_model_value(ENGINE, &ctx.model);
    let layout = super::layout::config_layout(ENGINE, &ctx.config_dir);
    let (rules, skills) = (layout.rules, layout.skills);
    let mut doc = json!({
        "$schema": "https://opencode.ai/config.json",
        "share": "disabled",
        "lsp": false,
        "enabled_providers": [PROVIDER],
        "model": model,
        "small_model": model,
        "instructions": [format!("{}/*.md", rules.display())],
        "skills": { "paths": [skills.display().to_string()] },
        "permission": permission(line),
        "provider": { PROVIDER: provider(ctx) },
    });
    match line {
        Line::V1 => {
            doc["autoupdate"] = json!(false);
            doc["snapshot"] = json!(false);
        }
        Line::V2 => {
            doc["update"] = json!("disable");
            doc["snapshots"] = json!(false);
        }
    }
    if let Some(c) = compaction(ctx) {
        doc["compaction"] = c;
    }
    if let (Line::V2, Some(ms)) = (line, ctx.command_limit_ms) {
        // The first line gets nothing for the limit: no profile pins it and nobody ran it with a plugin.
        doc["plugins"] = json!([{
            "package": shell_limit_dir(&layout_root(ctx)).display().to_string(),
            "options": { "timeoutMs": ms },
        }]);
    }
    doc
}

/// The harness's root of this engine for the context's config directory.
fn layout_root(ctx: &EmitContext) -> PathBuf {
    super::layout::config_layout(ENGINE, &ctx.config_dir).root
}

/// Writes the session document into the harness's root and names what the launch adds to the engine's environment. The profile already carries
/// the engine's kill switches and its folders; these are the ones that belong with the document.
pub fn emit(ctx: &EmitContext, line: Line) -> io::Result<Emitted> {
    let doc = document(ctx, line);
    let layout = super::layout::config_layout(ENGINE, &ctx.config_dir);
    let path = layout.root.join("opencode.json");
    write(&path, &pretty(&doc))?;
    // A repository's own opencode.json or .opencode folder is not read at all (it is also moved aside before launch). Both lines know the name.
    let mut env: BTreeMap<String, String> = BTreeMap::from([(
        "OPENCODE_DISABLE_PROJECT_CONFIG".to_string(),
        "1".to_string(),
    )]);
    if line == Line::V1 {
        // Names only the first line holds: its default plugins are installed from a package registry at start, which the firewall drops, and it
        // asks min(the model's limit, this) output tokens. The second line has neither name, so it is not given them.
        env.insert("OPENCODE_DISABLE_DEFAULT_PLUGINS".into(), "1".into());
        if let Some(n) = ctx.max_output_tokens {
            env.insert(
                "OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX".into(),
                n.to_string(),
            );
        }
    }
    env.insert(CONFIG_FILE.into(), path.display().to_string());
    env.insert(CONFIG_CONTENT.into(), doc.to_string());
    let mut files = vec![path];
    if let (Line::V2, Some(_)) = (line, ctx.command_limit_ms) {
        // The plugin's text beside the document, in the root the engine's user cannot write. A link where a folder belongs is refused: root
        // would otherwise write behind it.
        let dir = shell_limit_dir(&layout.root);
        if let Some(parent) = dir.parent() {
            super::direct::refuse_symlink(parent)?;
        }
        super::direct::refuse_symlink(&dir)?;
        let plugin = dir.join("index.js");
        super::direct::refuse_symlink(&plugin)?;
        write(&plugin, SHELL_LIMIT_PLUGIN)?;
        files.push(plugin);
        env.insert(PRINT_LOGS.0.into(), PRINT_LOGS.1.into());
        env.insert(LOG_LEVEL.0.into(), LOG_LEVEL.1.into());
    }
    Ok(Emitted { files, env })
}

/// Whether `doc` names the plugin, and only it, with the limit `ms`: a `plugins` member of exactly one entry whose `package` is `dir` and whose
/// `options.timeoutMs` is `ms`.
fn names_plugin(doc: &Value, dir: &Path, ms: u64) -> bool {
    let Some([entry]) = doc
        .get("plugins")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return false;
    };
    entry.get("package").and_then(Value::as_str) == dir.to_str()
        && entry.pointer("/options/timeoutMs").and_then(Value::as_u64) == Some(ms)
}

/// The read-back of the command time limit: what [`emit`] wrote is read again before the engine starts. The inline document and the document in
/// the file the launch names must each hold the `plugins` member with exactly the one entry (the plugin's folder under the harness's root and
/// `ms`), and the plugin file must be a regular file whose bytes are the compiled-in text. The two log variables are diagnostics and are not
/// looked at. Every failure is the one sentence, which names no value and no path.
pub fn check_limit(
    config_dir: &Path,
    env: &BTreeMap<String, String>,
    ms: u64,
) -> Result<(), String> {
    let bad = || LIMIT_NOT_HELD.to_string();
    let dir = shell_limit_dir(&super::layout::config_layout(ENGINE, config_dir).root);
    let inline: Value = env
        .get(CONFIG_CONTENT)
        .and_then(|t| serde_json::from_str(t).ok())
        .ok_or_else(bad)?;
    let on_disk: Value = env
        .get(CONFIG_FILE)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(bad)?;
    if !names_plugin(&inline, &dir, ms) || !names_plugin(&on_disk, &dir, ms) {
        return Err(bad());
    }
    let plugin = dir.join("index.js");
    let regular = std::fs::symlink_metadata(&plugin).is_ok_and(|m| m.file_type().is_file());
    if !regular || std::fs::read(&plugin).ok().as_deref() != Some(SHELL_LIMIT_PLUGIN.as_bytes()) {
        return Err(bad());
    }
    Ok(())
}

/// The line the engine's error log holds when this launch's command time limit is in force: the plugin's ready line with the limit the inline
/// document gives it. `None` for a launch that names no such plugin or lacks one of the two log variables (the line would then never be seen).
pub fn ready_line(env: &BTreeMap<String, String>) -> Option<String> {
    ready_ms(env).map(|ms| format!("{READY_PREFIX}{ms}"))
}

/// The limit of [`ready_line`], in milliseconds.
pub fn ready_ms(env: &BTreeMap<String, String>) -> Option<u64> {
    let logs = [PRINT_LOGS, LOG_LEVEL]
        .iter()
        .all(|(k, v)| env.get(*k).map(String::as_str) == Some(*v));
    if !logs {
        return None;
    }
    let doc: Value = serde_json::from_str(env.get(CONFIG_CONTENT)?).ok()?;
    let ms = doc
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|e| {
            e.get("package")
                .and_then(Value::as_str)
                .is_some_and(|p| p.ends_with(SHELL_LIMIT_DIR))
        })?
        .pointer("/options/timeoutMs")?
        .as_u64()?;
    Some(ms)
}

/// The file layer of the configuration: the variable that names the document on disk.
const CONFIG_FILE: &str = "OPENCODE_CONFIG";

/// Writes the two knobs of a local stand-in run into both layers of the document (the file `OPENCODE_CONFIG` names and the inline layer):
/// `provider.amazon-bedrock.options.baseURL`, the model endpoint, and `experimental.location_idle_timeout`, how long the engine keeps a folder's
/// services without session activity.
pub fn stand_in(
    env: &mut BTreeMap<String, String>,
    endpoint: Option<&str>,
    idle_timeout: Option<&str>,
) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let point = |text: &str| -> io::Result<Value> {
        let mut doc: Value =
            serde_json::from_str(text).map_err(|_| bad("configuration is not JSON"))?;
        if let Some(url) = endpoint {
            doc["provider"][PROVIDER]["options"]["baseURL"] = json!(url);
        }
        if let Some(idle) = idle_timeout {
            doc["experimental"]["location_idle_timeout"] = json!(idle);
        }
        Ok(doc)
    };
    let inline = env
        .get(CONFIG_CONTENT)
        .ok_or_else(|| bad("the launch has no inline configuration"))?;
    let doc = point(inline)?;
    env.insert(CONFIG_CONTENT.into(), doc.to_string());
    let path = env
        .get(CONFIG_FILE)
        .ok_or_else(|| bad("the launch names no configuration file"))?;
    let on_disk = point(&std::fs::read_to_string(path)?)?;
    std::fs::write(path, pretty(&on_disk))
}

/// The `mcp` member for the tool server at `url`, from the profile's template (the v1 form), in the form of `line`. The second line keeps
/// its tool servers under `mcp.servers` and offers their tools through "Code Mode", a single `execute` tool, unless a server says
/// `codemode: false`; the harness tool must be an ordinary tool of the engine, as it is for the first engine, so the entry says so.
fn registered(line: Line, template: &str, url: &str) -> io::Result<Value> {
    let v1 = registered_v1(template, url)?;
    if line == Line::V1 {
        return Ok(v1);
    }
    let mut servers = Map::new();
    for (name, entry) in v1.as_object().into_iter().flatten() {
        let ms = entry
            .get("timeout")
            .cloned()
            .unwrap_or_else(|| json!(120_000));
        servers.insert(
            name.clone(),
            json!({
                "type": "remote",
                "url": entry.get("url").cloned().unwrap_or(Value::Null),
                "oauth": false,
                "codemode": false,
                "timeout": { "catalog": ms, "execution": ms },
            }),
        );
    }
    Ok(json!({ "servers": servers }))
}

/// The first line of a prompt that would otherwise be read as one of the engine's commands.
const PLAIN_TASK_HEADER: &str = "Task (plain text, not a command):";

/// Whether the engine would read `text` as a command: it trims the text and a command starts with `/` (the trim of its language also
/// removes the byte order mark). A command word the engine does not advertise runs as plain text; the guard does not tell which words are
/// advertised, because the list is the engine's and changes with its release (`init`, `review`, `goal` and `compact` on the pinned one).
fn reads_as_command(text: &str) -> bool {
    text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .starts_with('/')
}

/// The text of a prompt as it goes on the wire. A prompt that starts with `/` after the engine's trim gets a plain line of its own before it,
/// so a task a user began with `/goal`, `/init`, `/review` or `/compact` reaches the model as a task and starts no command. Any other text is
/// returned as it came, byte for byte.
pub fn guard_prompt(text: &str) -> Cow<'_, str> {
    if reads_as_command(text) {
        Cow::Owned(format!("{PLAIN_TASK_HEADER}\n{text}"))
    } else {
        Cow::Borrowed(text)
    }
}

/// The entries of the tool server's registration template for `url`, as the tools module renders it.
fn registered_v1(template: &str, url: &str) -> io::Result<Value> {
    let text = template
        .replace("{toolsName}", harness_protocol::tools::SERVER_NAME)
        .replace("{toolsUrl}", url);
    if text.contains("{tools") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "placeholder left in the template",
        ));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "template is not JSON"))?;
    v.get("mcp")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "template has no mcp member"))
}

/// The permission that closes the tool a stage kill switch (`AGENTD_DISABLED_TOOLS`, `tools.disable`) names. The names are the first engine's;
/// a name this arm has no tool for is ignored.
fn permission_of_tool(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "spawn_agent" => Some(&["task"]),
        "fetch_web_content" => Some(&["webfetch"]),
        _ => None,
    }
}

/// What the launch adds once the modules ran and the stage's kill switches are known (`line` is the release line of the profile): the tool server's entry, when the server is up, and a
/// closed permission for each tool a kill switch names. Both go to the inline layer only, which outranks every file; the document on disk is
/// what it was. With nothing to add the environment is not touched.
pub fn finalize_env(
    line: Line,
    template: Option<&str>,
    disabled: &[String],
    env: &mut BTreeMap<String, String>,
) -> io::Result<()> {
    let Some(text) = env.get(CONFIG_CONTENT) else {
        return Ok(());
    };
    let mut doc: Value = serde_json::from_str(text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "inline configuration is not JSON",
        )
    })?;
    let mut changed = false;
    if let (Some(template), Some(url)) = (template, env.get(TOOLS_URL).filter(|u| !u.is_empty())) {
        doc["mcp"] = registered(line, template, url)?;
        changed = true;
    }
    for name in disabled {
        for key in permission_of_tool(name).into_iter().flatten() {
            doc["permission"][*key] = json!("deny");
            changed = true;
        }
    }
    if changed {
        env.insert(CONFIG_CONTENT.into(), doc.to_string());
    }
    Ok(())
}
