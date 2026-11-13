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
    doc
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
    env.insert("OPENCODE_CONFIG".into(), path.display().to_string());
    env.insert(CONFIG_CONTENT.into(), doc.to_string());
    Ok(Emitted {
        files: vec![path],
        env,
    })
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
