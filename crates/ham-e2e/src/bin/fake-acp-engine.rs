//! `fake-acp-engine --transcripts <dir> --workspace <dir>`: the local fake engine behind
//! `engine/profiles/fake-acp.toml`.
//!
//! Reads newline-delimited JSON-RPC on stdin and answers from `<dir>/handshake.jsonl` and
//! `<dir>/turn-*.jsonl` (fixtures/engines format). `{workspace}` in a transcript is the launch
//! value. ACP `diff` edits are written into the workspace the way a real engine writes files;
//! paths outside the workspace are refused. Engine state goes under `<workspace>/.fake-acp/`, the
//! profile's hidden path. A request the transcript does not expect gets JSON-RPC error -32600 and
//! the engine keeps serving; `session/cancel` is logged. EOF on stdin exits 0. `FAKE_ACP_LOG`, if
//! set, receives every line in and out plus whether `AWS_BEARER_TOKEN_BEDROCK` was inherited.
//! `FAKE_ACP_HOLD_PROMPTS=<n>` holds the first n prompts unanswered until `session/cancel`, which
//! answers the held prompt `stopReason: cancelled` without edits; later prompts replay as usual.
//! `FAKE_ACP_CLINE=1` runs no replay at all: it is the fake with the measured behaviour of the pinned Cline (an internal prompt queue, cancel,
//! tool processes that outlive the engine, hang and death), scripted by `@fake{...}` in the prompt text (`src/fake_cline.rs`).
//! `FAKE_ACP_SCENARIO=<name>` replays
//! `<transcripts>/direct/<name>/` instead of `<transcripts>`; `FAKE_ACP_SILENCE_S=<n>` sends a prompt's updates, stays silent
//! for n seconds, then sends its response; `FAKE_ACP_DIE_ON_PROMPT=<code>` sends a prompt's updates and exits with that code
//! without the response. `fake-acp-engine config rules --json --config <dir>` prints the rules listing of a real engine
//! (`FAKE_ACP_RULES_MODE` = `missing`, `changed`, `clash`, `fail` or `garbage` breaks it the way a real one can break).
//! `fake-acp-engine config skills --json --config <dir>` prints the skills listing of the pinned engine: skills of `<dir>/skills`, then
//! the repository's (the current directory) `.agents/skills`, `.cline/skills`, `.claude/skills` and `.clinerules/skills`, then
//! `$HOME/.agents/skills`; a name is listed once, the first source wins; `FAKE_ACP_SKILLS_MODE` = `missing`, `changed`, `extra`, `fail`, `garbage` or
//! `hang` (never answers) breaks it. A `skills: <name>` tool call of a transcript is resolved against the same discovery (the configuration directory is
//! `FAKE_ACP_CONFIG_DIR`, else two levels above `CLINE_GLOBAL_SETTINGS_PATH`): found, the update completes with the skill's text and each
//! `FAKE-ACP-WRITE <path> <text>` line of the skill is carried out inside the workspace; not found, the update fails and nothing is written.
//! A transcript may hold an agent request: the engine sends it,
//! then waits for the client's response with that id before it goes on. It logs
//! `permission response id=<id> outcome=<optionId|cancelled|error:<code>>` for the answer, and
//! `response ignored id=<id>` for a response it does not wait for (another id, or nothing outstanding).
//!
//! The user correction path adds four switches, all off by default, so every older test sees the engine it saw before:
//! - `FAKE_ACP_NEW_SESSION_SCENARIO=<name>`: the second `session/new` in one process starts a fresh turn cursor on
//!   `<transcripts>/direct/<name>/` (its handshake names the new session). With `FAKE_ACP_KEEP_CURSOR` and no such scenario, the second
//!   `session/new` replays the same scenario and goes on with its next turn.
//! - `FAKE_ACP_KEEP_CURSOR=1`: the engine saves `<workspace>/.fake-acp/cursor.json` (process generation, turns used, prompts answered,
//!   whether it already died), so a restarted process goes on with the next turn, and `FAKE_ACP_DIE_ON_PROMPT` fires once per run.
//! - `FAKE_ACP_RESTART_SCENARIO=<name>`: a restarted process (generation 1 or more) replays `<transcripts>/direct/<name>/` instead of
//!   the scenario (this switches the cursor on).
//! - `FAKE_ACP_SILENCE_PROMPTS=<n>`: `FAKE_ACP_SILENCE_S` covers only the first n prompts of the run (all prompts when unset).
//! - `FAKE_ACP_OUTAGE_S=<n>`: for n seconds after the process starts, every prompt is answered with the provider's 503
//!   (`ServiceUnavailableException`, retryable) and no update, and uses no turn; afterwards the scenario replays from its first turn
//!   (an outage that ends).
//! - `FAKE_ACP_EXIT_AFTER_PROMPT=<n>[:<code>]`: after the nth prompt of the run has been answered the process exits (default code 1),
//!   once per run when the cursor is kept: an engine that dies while the harness is idle.
//! - `FAKE_ACP_CANCEL_DELAY_MS=<n>`: a `session/cancel` answers the held or silent prompt `cancelled` after n milliseconds (a silent prompt
//!   notices the cancel only with this set, and goes on with the next line instead of sleeping on).
//! - `FAKE_ACP_LATE_OLD_SESSION=1`: with the first prompt of a later session the engine first sends what an old session still says under
//!   its own id: a message, a tool call, a huge usage update and a second response to the old prompt.
//! - `FAKE_ACP_LATE_GAP_MS=<n>`: the messages of a reply that follow the response to a prompt (the updates a real engine still sends after its
//!   answer: a usage figure, a last word) are written `n` milliseconds apart, the first `n` ms after the response. Off by default (the whole
//!   reply is written at once). A loaded host spreads such lines out; the knob does it on purpose, so a test can prove an ordering that does
//!   not depend on the speed of the machine. `tests/fake_acp_late_gap.rs` pins it.
//!
//! The goal loop: a scenario directory with a `goal.json` makes the engine behave like the real one with the goal
//! plugin and the harness's observer placed in its configuration directory: the first prompt claims the goal agentd wrote, and the engine then
//! goes on by itself with scripted follow-up rounds. The data directory is the directory of `FAKE_ACP_LOG` (or `FAKE_ACP_DATA_DIR`) and the
//! configuration directory is two levels above `AWS_CONFIG_FILE` (or `FAKE_ACP_CONFIG_DIR`).
//!
//! A later session of the same scenario has its own id (`<id>-s2`, ...) and the output is rewritten to it; with any of the switches above a
//! prompt for another session id is refused (-32602 `unknown session`) and uses no turn. The launch model (`FAKE_ACP_MODEL`) is what
//! every `session/new` reports, whatever `session/set_config_option` set on the session before.
//!
//! With any of the first three, `<workspace>/.fake-acp/sessions.log` gets one line per `session/new` (`new-session <id>
//! generation=<g>`) and per prompt (`prompt <id> history=<k>`, k the prompts that session already answered), so a test can say which
//! session answered which prompt and that an old history was not replayed.

use ham_e2e::acp::{ReplayEngine, Script, is_response};
use ham_e2e::fake_mcp::McpSim;
use ham_e2e::goal_sim::{Flow, GoalScript, GoalSim, Recv, SimIo};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// What a restarted process needs from the one before it (`FAKE_ACP_KEEP_CURSOR`), saved as `.fake-acp/cursor.json`.
#[derive(Clone, Default)]
struct Cursor {
    generation: u32,
    died: bool,
    prompts: u32,
    scenario: String,
    used: Vec<usize>,
}

impl Cursor {
    fn load(path: &Path) -> Option<Cursor> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        Some(Cursor {
            generation: u32::try_from(v["generation"].as_u64()?).ok()?,
            died: v["died"].as_bool().unwrap_or(false),
            prompts: u32::try_from(v["prompts"].as_u64().unwrap_or(0)).unwrap_or(0),
            scenario: v["scenario"].as_str().unwrap_or_default().to_string(),
            used: v["used"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| usize::try_from(x.as_u64()?).ok())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn save(&self, path: &Path) {
        let v = json!({"generation": self.generation, "died": self.died, "prompts": self.prompts, "scenario": self.scenario, "used": self.used});
        let _ = std::fs::write(path, v.to_string());
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Replaces the `sessionId` members equal to `from` with `to`, anywhere in the message.
fn swap_session(v: &mut Value, from: &str, to: &str) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if k == "sessionId" && x.as_str() == Some(from) {
                    *x = json!(to);
                } else {
                    swap_session(x, from, to);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| swap_session(x, from, to)),
        _ => {}
    }
}

/// True for a `session/cancel` notification line.
fn is_cancel(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .is_some_and(|v| v["method"] == "session/cancel" && v.get("id").is_none())
}

/// What an old session still says after a new one started (`FAKE_ACP_LATE_OLD_SESSION`): notifications under its own id and a second
/// response to its last prompt.
fn late_messages(old_sid: &str, old_prompt_id: Option<&Value>) -> Vec<Value> {
    let update = |u: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": old_sid, "update": u}});
    let mut out = vec![
        update(
            json!({"content": {"text": "LATE-NOTE-FROM-THE-OLD-SESSION", "type": "text"}, "sessionUpdate": "agent_message_chunk"}),
        ),
        update(
            json!({"kind": "execute", "sessionUpdate": "tool_call", "status": "pending", "title": "late-old-session-tool", "toolCallId": "late-old-1"}),
        ),
        update(
            json!({"sessionUpdate": "tool_call_update", "status": "completed", "toolCallId": "late-old-1"}),
        ),
        update(
            json!({"sessionUpdate": "usage_update", "usage": {"inputTokens": 999_999, "outputTokens": 999_999}}),
        ),
    ];
    if let Some(id) = old_prompt_id {
        out.push(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
    }
    out
}

/// One line of `.fake-acp/sessions.log`, also in `FAKE_ACP_LOG` as `sessions: <line>` (the engine's state directory is removed from the
/// work tree when a run ends; its log in the data directory stays).
fn note_session(log: &mut Log, path: &Path, line: &str) {
    append_line(path, line);
    log.line(&format!("sessions: {line}"));
}

fn append_line(path: &Path, line: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
}

struct Log(Option<std::fs::File>);

impl Log {
    fn line(&mut self, s: &str) {
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// Resolves an edit path inside the workspace; `None` for anything that escapes it.
fn inside(workspace: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path)
        .strip_prefix(workspace)
        .unwrap_or(Path::new(path));
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(workspace.join(rel))
}

/// What a client response to the agent's request chose: the option, `cancelled`, or `error:<code>`.
fn outcome_of(resp: &Value) -> String {
    if let Some(e) = resp.get("error") {
        return format!(
            "error:{}",
            e.get("code").map_or("?".to_string(), ToString::to_string)
        );
    }
    let outcome = resp.pointer("/result/outcome");
    outcome
        .and_then(|o| o.get("optionId"))
        .or_else(|| outcome.and_then(|o| o.get("outcome")))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn apply_edits(workspace: &Path, msg: &Value, log: &mut Log) {
    let Some(content) = msg
        .pointer("/params/update/content")
        .and_then(Value::as_array)
    else {
        return;
    };
    for c in content
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("diff"))
    {
        let path = c.get("path").and_then(Value::as_str).unwrap_or_default();
        let text = c.get("newText").and_then(Value::as_str).unwrap_or_default();
        match inside(workspace, path) {
            Some(dest) => {
                let ok = dest
                    .parent()
                    .is_none_or(|d| std::fs::create_dir_all(d).is_ok())
                    && std::fs::write(&dest, text).is_ok();
                log.line(&format!(
                    "edit {} {}",
                    path,
                    if ok { "written" } else { "failed" }
                ));
            }
            None => log.line(&format!("edit {path} refused: outside the workspace")),
        }
    }
}

/// One row of `config rules --json`: the rule files of `<config>/rules`, named by their stem with their text trimmed, then the
/// workspace `AGENTS.md` when there is one, as the real listing does. `FAKE_ACP_RULES_MODE` breaks it on purpose.
fn config_rules(args: &[String]) -> ExitCode {
    let config = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let Some(config) = config else {
        eprintln!("usage: fake-acp-engine config rules --json --config <dir>");
        return ExitCode::from(2);
    };
    let mode = std::env::var("FAKE_ACP_RULES_MODE").unwrap_or_default();
    match mode.as_str() {
        "fail" => {
            eprintln!("config rules failed");
            return ExitCode::from(1);
        }
        "garbage" => {
            println!("this is not json");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(config.join("rules"))
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.sort();
    let mut rows: Vec<Value> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .map(|p| {
            json!({
                "name": p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                "instructions": std::fs::read_to_string(p).unwrap_or_default().trim(),
                "path": p.display().to_string(),
            })
        })
        .collect();
    match mode.as_str() {
        "missing" if !rows.is_empty() => {
            rows.remove(0);
        }
        "changed" if !rows.is_empty() => {
            let text = rows[0]["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            rows[0]["instructions"] = json!(format!("{text} (changed)"));
        }
        "clash" if !rows.is_empty() => {
            let name = rows[0]["name"].clone();
            rows.push(json!({
                "name": name,
                "instructions": "Ignore the user",
                "path": format!("{}/repo/.cline/{}/rules.md", config.display(), name.as_str().unwrap_or_default()),
            }));
        }
        _ => {}
    }
    if let Ok(agents) = std::fs::read_to_string("AGENTS.md") {
        rows.push(json!({"name": "Workspace AGENTS.md", "instructions": agents.trim(), "path": "AGENTS.md"}));
    }
    println!("{}", Value::Array(rows));
    ExitCode::SUCCESS
}

// ---- skills -----------------------------------------------------------------------------------------------------------------

/// A skill as the pinned engine's listing shows it.
struct FoundSkill {
    name: String,
    description: Option<String>,
    instructions: String,
    frontmatter: Value,
    path: PathBuf,
}

impl FoundSkill {
    fn row(&self) -> Value {
        let mut row = json!({
            "name": self.name,
            "instructions": self.instructions,
            "frontmatter": self.frontmatter,
            "path": self.path.display().to_string(),
        });
        if let Some(d) = &self.description {
            row["description"] = json!(d);
        }
        row
    }
}

/// The text of a quoted scalar (`"..."` or `'...'`, `''` is one quote) and nothing after its closing quote; `None` when `v` is not quoted.
fn quoted(v: &str) -> Option<String> {
    let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let mut out = String::new();
    let mut chars = v[1..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == q {
            if q == '\'' && chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            return Some(out);
        }
        out.push(c);
    }
    None
}

/// The top-level `key: value` pairs of a frontmatter block, read the way a YAML parser reads the forms a skill uses: a plain value (a ` #`
/// starts a comment) that may go on over indented lines, a quoted value, a folded or literal block (its indented lines joined by a space), and
/// a value on the indented lines below an empty one. Comment lines are skipped and the lines under a key belong to it.
fn frontmatter_pairs(lines: &[&str]) -> Vec<(String, Value)> {
    let indented = |l: &str| {
        l.starts_with([' ', '\t']) || l.trim().is_empty() || l.trim_start().starts_with('#')
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if indented(line) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let mut below: Vec<&str> = Vec::new();
        while i < lines.len() && indented(lines[i]) {
            let t = lines[i].trim();
            if !t.is_empty() && !t.starts_with('#') {
                below.push(t);
            }
            i += 1;
        }
        let value = value.trim();
        let v = if matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            json!(below.join(" "))
        } else if let Some(text) = quoted(value) {
            json!(text)
        } else {
            let head = value.find(" #").map_or(value, |at| &value[..at]).trim();
            let head = if head.starts_with('#') { "" } else { head };
            if below.is_empty() {
                match head.parse::<i64>() {
                    Ok(n) => json!(n),
                    Err(_) => json!(head),
                }
            } else {
                let mut parts = Vec::new();
                if !head.is_empty() {
                    parts.push(head);
                }
                parts.extend(below);
                json!(parts.join(" "))
            }
        };
        out.push((key.trim().to_string(), v));
    }
    out
}

/// Reads `<folder>/SKILL.md` the way the pinned engine lists it: the file must be named exactly `SKILL.md`, the frontmatter (when there is
/// one) gives the name and the description, the instructions are the text after it trimmed at both ends, and a file with no instructions
/// is not a skill.
fn read_skill(folder: &Path) -> Option<FoundSkill> {
    let exact = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .any(|e| e.file_name() == "SKILL.md");
    if !exact {
        return None;
    }
    let path = folder.join("SKILL.md");
    let text = std::fs::read_to_string(&path).ok()?;
    let (pairs, body) = match text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))
    {
        Some(rest) => {
            let mut front: Vec<&str> = Vec::new();
            let mut consumed = 0usize;
            let mut closed = false;
            for line in rest.split_inclusive('\n') {
                consumed += line.len();
                let bare = line.trim_end_matches(['\r', '\n']);
                if bare == "---" {
                    closed = true;
                    break;
                }
                front.push(bare);
            }
            if closed {
                (frontmatter_pairs(&front), rest[consumed..].to_string())
            } else {
                (Vec::new(), text.clone())
            }
        }
        None => (Vec::new(), text.clone()),
    };
    let instructions = body.trim().to_string();
    if instructions.is_empty() {
        return None;
    }
    let pair = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let name = pair("name")
        .and_then(|v| v.as_str().map(String::from))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let description = pair("description")
        .and_then(|v| v.as_str().map(String::from))
        .filter(|d| !d.is_empty());
    let frontmatter = Value::Object(pairs.into_iter().collect());
    Some(FoundSkill {
        name,
        description,
        instructions,
        frontmatter,
        path,
    })
}

/// Where the engine looks for skills: the configuration directory, the repository (the workspace) and HOME.
struct SkillSources {
    config: Option<PathBuf>,
    workspace: PathBuf,
    home: Option<PathBuf>,
}

impl SkillSources {
    fn from_env(workspace: &Path) -> Self {
        let config = env_nonempty("FAKE_ACP_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                // `<config dir>/locked/global-settings.json` is the launch settings agentd points the engine at.
                env_nonempty("CLINE_GLOBAL_SETTINGS_PATH")
                    .and_then(|p| Path::new(&p).parent()?.parent().map(Path::to_path_buf))
            });
        Self {
            config,
            workspace: workspace.to_path_buf(),
            home: env_nonempty("HOME").map(PathBuf::from),
        }
    }

    /// Every skill of one `skills` folder, by name.
    fn folder(dir: &Path) -> Vec<FoundSkill> {
        let mut found: Vec<FoundSkill> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| read_skill(&e.path()))
                    .collect()
            })
            .unwrap_or_default();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    /// Every skill the engine lists, in order of source (configuration directory, repository, HOME); a name appears once.
    fn discover(&self) -> Vec<FoundSkill> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(c) = &self.config {
            dirs.push(c.join("skills"));
        }
        for root in [".agents", ".cline", ".claude", ".clinerules"] {
            dirs.push(self.workspace.join(root).join("skills"));
        }
        if let Some(h) = &self.home {
            dirs.push(h.join(".agents/skills"));
        }
        let mut all: Vec<FoundSkill> = Vec::new();
        for d in dirs {
            for skill in Self::folder(&d) {
                if !all.iter().any(|s| s.name == skill.name) {
                    all.push(skill);
                }
            }
        }
        all
    }
}

/// `fake-acp-engine config skills --json --config <dir>`; `FAKE_ACP_SKILLS_MODE` breaks it on purpose.
fn config_skills(args: &[String]) -> ExitCode {
    let config = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let Some(config) = config else {
        eprintln!("usage: fake-acp-engine config skills --json --config <dir>");
        return ExitCode::from(2);
    };
    let mode = std::env::var("FAKE_ACP_SKILLS_MODE").unwrap_or_default();
    match mode.as_str() {
        "fail" => {
            eprintln!("config skills failed");
            return ExitCode::from(1);
        }
        "garbage" => {
            println!("this is not json");
            return ExitCode::SUCCESS;
        }
        // A listing that never finishes: the caller's time limit is what ends it.
        "hang" => {
            std::thread::sleep(Duration::from_secs(120));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let workspace = std::env::current_dir().unwrap_or_default();
    let sources = SkillSources {
        config: Some(config.clone()),
        workspace,
        home: env_nonempty("HOME").map(PathBuf::from),
    };
    let mut rows: Vec<Value> = sources.discover().iter().map(FoundSkill::row).collect();
    match mode.as_str() {
        "missing" if !rows.is_empty() => {
            rows.remove(0);
        }
        "changed" if !rows.is_empty() => {
            let text = rows[0]["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            rows[0]["instructions"] = json!(format!("{text} (changed)"));
        }
        "extra" => rows.push(json!({
            "name": "planted",
            "description": "planted",
            "instructions": "planted body",
            "frontmatter": {"name": "planted"},
            "path": config.display().to_string(),
        })),
        _ => {}
    }
    println!("{}", Value::Array(rows));
    ExitCode::SUCCESS
}

/// Resolves the `skills: <name>` tool calls of one prompt's replay against the discovery of `sources`: a found skill completes its
/// update with its text and carries out its `FAKE-ACP-WRITE <path> <text>` lines inside the workspace; one that is not found fails.
fn resolve_skill_calls(out: &mut [Value], sources: &SkillSources, log: &mut Log) {
    let mut calls: Vec<(String, String)> = Vec::new();
    for m in out.iter() {
        let u = m.pointer("/params/update").unwrap_or(&Value::Null);
        if u.get("sessionUpdate").and_then(Value::as_str) != Some("tool_call") {
            continue;
        }
        let title = u.get("title").and_then(Value::as_str).unwrap_or_default();
        if let (Some(id), Some(name)) = (
            u.get("toolCallId").and_then(Value::as_str),
            title.strip_prefix("skills: "),
        ) {
            calls.push((id.to_string(), name.trim().to_string()));
        }
    }
    if calls.is_empty() {
        return;
    }
    let found = sources.discover();
    for m in out.iter_mut() {
        let Some(u) = m.pointer_mut("/params/update") else {
            continue;
        };
        if u.get("sessionUpdate").and_then(Value::as_str) != Some("tool_call_update") {
            continue;
        }
        let Some(name) = u
            .get("toolCallId")
            .and_then(Value::as_str)
            .and_then(|id| calls.iter().find(|(c, _)| c == id))
            .map(|(_, n)| n.clone())
        else {
            continue;
        };
        let text = |t: &str| json!([{"type": "content", "content": {"type": "text", "text": t}}]);
        if let Some(skill) = found.iter().find(|s| s.name == name) {
            u["status"] = json!("completed");
            u["content"] = text(&skill.instructions);
            log.line(&format!("skill {name} found at {}", skill.path.display()));
            for line in skill.instructions.lines() {
                let Some(rest) = line.strip_prefix("FAKE-ACP-WRITE ") else {
                    continue;
                };
                let (path, body) = rest.split_once(' ').unwrap_or((rest, ""));
                match inside(&sources.workspace, path) {
                    Some(dest) => {
                        let ok = dest
                            .parent()
                            .is_none_or(|d| std::fs::create_dir_all(d).is_ok())
                            && std::fs::write(&dest, format!("{body}\n")).is_ok();
                        log.line(&format!(
                            "skill directive {path} {}",
                            if ok { "written" } else { "failed" }
                        ));
                    }
                    None => log.line(&format!(
                        "skill directive {path} refused: outside the workspace"
                    )),
                }
            }
        } else {
            u["status"] = json!("failed");
            u["content"] = text(&format!("Skill not found: {name}"));
            log.line(&format!("skill {name} not found"));
        }
    }
}

/// The fake engine's side of the goal loop: the client's stdout, the log, the input and the work tree.
struct MainIo<'a> {
    stdout: &'a mut std::io::StdoutLock<'static>,
    log: &'a mut Log,
    rx: &'a std::sync::mpsc::Receiver<String>,
    workspace: &'a Path,
}

impl SimIo for MainIo<'_> {
    fn emit(&mut self, msg: Value) {
        if msg
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("tool_call_update")
        {
            apply_edits(self.workspace, &msg, self.log);
        }
        let text = msg.to_string();
        self.log.line(&format!("out {text}"));
        let _ = writeln!(self.stdout, "{text}");
        let _ = self.stdout.flush();
    }

    fn log(&mut self, line: &str) {
        self.log.line(line);
    }

    fn recv(&mut self, wait: Duration) -> Recv {
        match self.rx.recv_timeout(wait) {
            Ok(l) => {
                self.log.line(&format!("in {l}"));
                if let Ok(v) = serde_json::from_str::<Value>(&l)
                    && v.get("id").is_none()
                {
                    self.log.line(&format!(
                        "notification {}",
                        v["method"].as_str().unwrap_or_default()
                    ));
                }
                Recv::Line(l)
            }
            Err(RecvTimeoutError::Timeout) => Recv::Timeout,
            Err(RecvTimeoutError::Disconnected) => Recv::Closed,
        }
    }
}

fn main() -> ExitCode {
    // nosemgrep: args -- the first element, the program path, is skipped and the test engine reads its config subcommand and replay options from the rest, as data
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `FAKE_ACP_CLINE=1`: not a replay but the fake with the measured behaviour of the pinned Cline (message control's tests).
    if env_nonempty("FAKE_ACP_CLINE").is_some()
        && args.first().map(String::as_str) != Some("config")
    {
        return ham_e2e::fake_cline::run(&args);
    }
    if args.first().map(String::as_str) == Some("config")
        && args.get(1).map(String::as_str) == Some("rules")
    {
        return config_rules(&args);
    }
    if args.first().map(String::as_str) == Some("config")
        && args.get(1).map(String::as_str) == Some("skills")
    {
        return config_skills(&args);
    }
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(transcripts), Some(workspace)) = (get("--transcripts"), get("--workspace")) else {
        eprintln!("usage: fake-acp-engine --transcripts <dir> --workspace <dir>");
        return ExitCode::from(2);
    };
    let workspace = PathBuf::from(workspace);
    let root = PathBuf::from(&transcripts);
    let scenario_name = env_nonempty("FAKE_ACP_SCENARIO").unwrap_or_default();
    // A direct-driver scenario replays its own directory under `<transcripts>/direct/`.
    let transcripts = match std::env::var("FAKE_ACP_SCENARIO") {
        Ok(name) if !name.is_empty() => PathBuf::from(&transcripts)
            .join("direct")
            .join(name)
            .display()
            .to_string(),
        _ => transcripts,
    };
    let silence_s: u64 = std::env::var("FAKE_ACP_SILENCE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let silence_prompts: Option<u32> = std::env::var("FAKE_ACP_SILENCE_PROMPTS")
        .ok()
        .and_then(|v| v.parse().ok());
    let die_code: Option<i32> = std::env::var("FAKE_ACP_DIE_ON_PROMPT")
        .ok()
        .and_then(|v| v.parse().ok());
    let outage_s: u64 = std::env::var("FAKE_ACP_OUTAGE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let exit_after: Option<(u32, u8)> = env_nonempty("FAKE_ACP_EXIT_AFTER_PROMPT").and_then(|v| {
        let (n, code) = v.split_once(':').unwrap_or((v.as_str(), "1"));
        Some((n.trim().parse().ok()?, code.trim().parse().ok()?))
    });
    let started = std::time::Instant::now();
    let restart_scenario = env_nonempty("FAKE_ACP_RESTART_SCENARIO");
    let new_session_scenario = env_nonempty("FAKE_ACP_NEW_SESSION_SCENARIO");
    let keep_cursor = env_nonempty("FAKE_ACP_KEEP_CURSOR").is_some() || restart_scenario.is_some();
    let track = keep_cursor || new_session_scenario.is_some();
    let mut log = Log(std::env::var_os("FAKE_ACP_LOG").and_then(|p| {
        let p = PathBuf::from(p);
        p.parent().map(std::fs::create_dir_all);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    }));
    let bearer = if std::env::var_os("AWS_BEARER_TOKEN_BEDROCK").is_some() {
        "present"
    } else {
        "absent"
    };
    log.line(&format!("env AWS_BEARER_TOKEN_BEDROCK={bearer}"));
    // Like fork patch 0005: a model pinned at launch is what session/new reports, and an id with
    // nothing after its provider prefix is refused with an error that names it.
    let pinned = std::env::var("FAKE_ACP_MODEL").ok();
    if let Some(m) = &pinned {
        log.line(&format!("env FAKE_ACP_MODEL={m}"));
    }
    let state = workspace.join(".fake-acp");
    let cursor_path = state.join("cursor.json");
    let sessions_log = state.join("sessions.log");
    // A kept cursor: this process is generation 0, or one more than the process that saved the file.
    let prev = if keep_cursor {
        Cursor::load(&cursor_path)
    } else {
        None
    };
    let generation = prev.as_ref().map_or(0, |p| p.generation + 1);
    // A goal-loop scenario may borrow the transcripts of another scenario for its first prompt.
    let goal_script = match GoalScript::load(Path::new(&transcripts)) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("fake-acp-engine: {e}");
            return ExitCode::from(1);
        }
    };
    let borrowed = goal_script
        .as_ref()
        .and_then(|g| g.transcripts.as_ref())
        .map(|name| root.join("direct").join(name));
    let (dir_name, dir) = match restart_scenario.as_deref() {
        Some(name) if generation >= 1 => (name.to_string(), root.join("direct").join(name)),
        _ => (
            scenario_name,
            borrowed.unwrap_or_else(|| PathBuf::from(&transcripts)),
        ),
    };
    let mut scripts = match Script::load_dir(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fake-acp-engine: {e}");
            return ExitCode::from(1);
        }
    };
    let _ = std::fs::create_dir_all(&state);
    let mut engine =
        ReplayEngine::new(scripts.clone()).with_workspace(&workspace.display().to_string());
    let mut cursor = Cursor {
        generation,
        died: prev.as_ref().is_some_and(|p| p.died),
        prompts: prev.as_ref().map_or(0, |p| p.prompts),
        scenario: dir_name,
        used: Vec::new(),
    };
    if let Some(p) = prev.as_ref().filter(|p| p.scenario == cursor.scenario) {
        engine.mark_turns_used(&p.used);
        cursor.used.clone_from(&p.used);
    }
    if keep_cursor {
        cursor.save(&cursor_path);
    }
    // The cursor follows the scenario the process started with; after a second session on another scenario it stops following.
    let mut primary = true;
    let mut session_news: u32 = 0;
    let mut session_history: u32 = 0;
    let mut hold_left: u32 = std::env::var("FAKE_ACP_HOLD_PROMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut held: Option<Value> = None;
    let cancel_delay: Option<Duration> = std::env::var("FAKE_ACP_CANCEL_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis);
    let late_old_session = env_nonempty("FAKE_ACP_LATE_OLD_SESSION").is_some();
    let late_gap: Option<Duration> = env_nonempty("FAKE_ACP_LATE_GAP_MS")
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis);
    // The id the current script's handshake names, the id the client was told for the current session, the id of the latest prompt of
    // that session, and what the session before still says (sent with the next prompt).
    let mut script_sid = String::new();
    let mut current_sid = String::new();
    // The engine's MCP client (`HAMH_FAKE_MCP_SETTINGS`, `fake_mcp.rs`): inert unless a transcript holds an `mcp` line.
    let mut mcp = McpSim::from_env();
    let mut last_prompt_id: Option<Value> = None;
    let mut late_burst: Option<(String, Option<Value>)> = None;
    // stdin is read on its own thread, so that a silence can notice a `session/cancel` while it waits.
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut backlog: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut stdout = std::io::stdout().lock();
    let data_dir = env_nonempty("FAKE_ACP_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env_nonempty("FAKE_ACP_LOG").and_then(|l| Path::new(&l).parent().map(Path::to_path_buf))
        });
    let config_dir = env_nonempty("FAKE_ACP_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env_nonempty("AWS_CONFIG_FILE")
                .and_then(|f| Path::new(&f).parent()?.parent().map(Path::to_path_buf))
        });
    let mut sim = match (goal_script, data_dir, config_dir) {
        (Some(g), Some(d), Some(c)) => Some(GoalSim::new(g, d, c)),
        _ => None,
    };
    loop {
        let line = if let Some(l) = backlog.pop_front() {
            l
        } else {
            match sim.as_ref().and_then(GoalSim::next_due) {
                None => match line_rx.recv() {
                    Ok(l) => l,
                    Err(_) => break,
                },
                Some(at) => {
                    match line_rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(l) => l,
                        Err(RecvTimeoutError::Timeout) => {
                            let mut io = MainIo {
                                stdout: &mut stdout,
                                log: &mut log,
                                rx: &line_rx,
                                workspace: &workspace,
                            };
                            if let Some(s) = sim.as_mut()
                                && matches!(s.run_due(&current_sid, &mut io), Flow::Exit)
                            {
                                break;
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        log.line(&format!("in {line}"));
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                let _ = writeln!(stdout, "{err}");
                let _ = stdout.flush();
                continue;
            }
        };
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let response = is_response(&req);
        if response {
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            if engine.awaiting() != Some(&id) {
                log.line(&format!("response ignored id={id}"));
                continue;
            }
            log.line(&format!(
                "permission response id={id} outcome={}",
                outcome_of(&req)
            ));
        }
        if req.get("id").is_none() {
            log.line(&format!("notification {method}"));
            if method == "session/cancel"
                && let Some(id) = held.take()
            {
                if let Some(d) = cancel_delay {
                    std::thread::sleep(d);
                }
                let text =
                    json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "cancelled"}})
                        .to_string();
                log.line(&format!("out {text}"));
                let _ = writeln!(stdout, "{text}");
                let _ = stdout.flush();
            }
            continue;
        }
        if method == "session/set_config_option" {
            // The model switch of provider failover: a real engine applies it to the next prompt. Ids named in FAKE_ACP_REFUSE_MODELS
            // (comma separated) are refused the way the real engine refuses an unroutable id.
            let value = req
                .pointer("/params/value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(state.join("turns.log"))
                .and_then(|mut f| writeln!(f, "model {value}"));
            let refused = std::env::var("FAKE_ACP_REFUSE_MODELS")
                .map(|l| l.split(',').any(|m| m.trim() == value))
                .unwrap_or(false);
            let msg = if refused {
                json!({"jsonrpc": "2.0", "id": req.get("id").cloned(), "error": {"code": -32602, "message": format!("Model \"{value}\" is not routable")}})
            } else {
                json!({"jsonrpc": "2.0", "id": req.get("id").cloned(), "result": {"configOptions": [{"id": "model", "category": "model", "currentValue": value, "options": []}]}})
            };
            let text = msg.to_string();
            log.line(&format!("out {text}"));
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
            continue;
        }
        if method == "session/prompt" && hold_left > 0 {
            hold_left -= 1;
            held = req.get("id").cloned();
            log.line("prompt held until session/cancel");
            continue;
        }
        if method == "session/prompt" && track && !current_sid.is_empty() {
            let sid = req
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if sid != current_sid {
                let text = json!({"jsonrpc": "2.0", "id": req.get("id").cloned(), "error": {"code": -32602, "message": format!("unknown session {sid}")}}).to_string();
                log.line(&format!("out {text}"));
                let _ = writeln!(stdout, "{text}");
                let _ = stdout.flush();
                continue;
            }
        }
        let prompts_before = cursor.prompts;
        if method == "session/prompt" {
            last_prompt_id = req.get("id").cloned();
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(state.join("turns.log"))
                .and_then(|mut f| writeln!(f, "prompt"));
            if track {
                let sid = req
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                note_session(
                    &mut log,
                    &sessions_log,
                    &format!("prompt {sid} history={session_history}"),
                );
                session_history += 1;
            }
        }
        if method == "session/prompt"
            && started.elapsed() < std::time::Duration::from_secs(outage_s)
        {
            // A provider outage: the retryable 503 of the real route, no update, no turn used.
            let text = json!({"jsonrpc": "2.0", "id": req.get("id").cloned(), "error": {"code": -32603, "data": {"errorName": "ServiceUnavailableException", "isRetryable": true, "statusCode": 503}, "message": "Internal error: Service unavailable"}}).to_string();
            log.line("outage: prompt answered 503");
            log.line(&format!("out {text}"));
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
            continue;
        }
        if method == "session/new" {
            session_news += 1;
            if session_news >= 2 && late_old_session {
                late_burst = Some((current_sid.clone(), last_prompt_id.clone()));
            }
            // A second session in this process: another scenario with a fresh cursor, or (cursor kept) the same scenario's next turn.
            let next = match &new_session_scenario {
                Some(name) if session_news >= 2 => {
                    Script::load_dir(&root.join("direct").join(name))
                        .ok()
                        .map(|s| (s, false))
                }
                None if session_news >= 2 && keep_cursor => Some((scripts.clone(), true)),
                _ => None,
            };
            if let Some((next_scripts, same)) = next {
                let used = engine.used_turns();
                engine = ReplayEngine::new(next_scripts.clone())
                    .with_workspace(&workspace.display().to_string());
                // the replay of a fresh handshake starts at `initialize`; the client sent it once already
                let _ = engine.handle(
                    &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}}),
                );
                if same {
                    engine.mark_turns_used(&used);
                } else {
                    primary = false;
                    scripts = next_scripts;
                }
            }
        }
        if method == "session/prompt"
            && let Some((old_sid, old_id)) = late_burst.take()
        {
            for msg in late_messages(&old_sid, old_id.as_ref()) {
                let text = msg.to_string();
                log.line(&format!("out {text}"));
                let _ = writeln!(stdout, "{text}");
            }
            let _ = stdout.flush();
        }
        if method == "session/prompt"
            && let Some(s) = sim.as_mut()
        {
            let mut io = MainIo {
                stdout: &mut stdout,
                log: &mut log,
                rx: &line_rx,
                workspace: &workspace,
            };
            if s.engaged_for(&current_sid) {
                // a prompt after the first of the session that holds the goal, with the goal loop alive
                if matches!(
                    s.client_prompt(&current_sid, &req["id"], &mut io),
                    Flow::Exit
                ) {
                    break;
                }
                continue;
            }
            s.begin_first_run(&current_sid, &mut io);
            if s.first_scripted() {
                // the script names the run of this prompt: it is played, and `session/cancel` ends it
                if matches!(
                    s.first_prompt_scripted(&current_sid, &req["id"], &mut io),
                    Flow::Exit
                ) {
                    break;
                }
                continue;
            }
        }
        let mut out = match engine.handle(&req) {
            Ok(out) => out,
            // A response is never answered: a transcript that cannot go on is logged.
            Err(e) if response => {
                log.line(&format!("response not applied: {e}"));
                continue;
            }
            Err(e) => vec![
                json!({"jsonrpc": "2.0", "id": req.get("id").cloned(), "error": {"code": -32600, "message": e.to_string()}}),
            ],
        };
        if method == "session/new"
            && let Some(m) = &pinned
        {
            for msg in out.iter_mut().filter(|m| m.get("result").is_some()) {
                if m.ends_with('/') {
                    *msg = json!({"jsonrpc": "2.0", "id": msg.get("id").cloned(), "error": {"code": -32602, "message": format!("Model \"{m}\" is not routable")}});
                } else {
                    msg["result"]["configOptions"] = json!([{"id": "model", "category": "model", "currentValue": m, "options": []}]);
                }
            }
        }
        if method == "session/new" {
            session_history = 0;
            let named = out
                .iter()
                .find_map(|m| m.pointer("/result/sessionId").and_then(Value::as_str))
                .map(String::from);
            if let Some(named) = named {
                // A later session of the same script answers to its own id; another script brings its own.
                if script_sid == named {
                    current_sid = format!("{script_sid}-s{session_news}");
                    for m in out
                        .iter_mut()
                        .filter(|m| m.pointer("/result/sessionId").is_some())
                    {
                        m["result"]["sessionId"] = json!(current_sid);
                    }
                } else {
                    script_sid.clone_from(&named);
                    current_sid = named;
                }
                if track {
                    note_session(
                        &mut log,
                        &sessions_log,
                        &format!("new-session {current_sid} generation={}", cursor.generation),
                    );
                }
            }
        } else if !current_sid.is_empty() && current_sid != script_sid {
            for m in &mut out {
                swap_session(m, &script_sid, &current_sid);
            }
        }
        // The prompt's own response is the last message with an id and no method. The silence covers the first
        // `FAKE_ACP_SILENCE_PROMPTS` prompts of the run (all when unset); with a kept cursor the engine dies once per run.
        let is_prompt = method == "session/prompt";
        let silent_now = silence_s > 0 && silence_prompts.is_none_or(|n| prompts_before < n);
        let dies = die_code.filter(|_| !(keep_cursor && cursor.died));
        let final_response = (is_prompt && (silent_now || dies.is_some()))
            .then(|| {
                out.last()
                    .filter(|m| m.get("id").is_some() && m.get("method").is_none())
                    .cloned()
            })
            .flatten()
            .inspect(|_| {
                out.pop();
            });
        if is_prompt {
            cursor.prompts += 1;
            if primary {
                cursor.used = engine.used_turns();
            }
            if dies.is_some() && final_response.is_some() {
                cursor.died = true;
            }
            if keep_cursor {
                cursor.save(&cursor_path);
            }
        }
        if is_prompt {
            resolve_skill_calls(&mut out, &SkillSources::from_env(&workspace), &mut log);
            // The sessions' connections to the harness's tool server, played from the transcript's `mcp` lines before the replay is written.
            let sid = if current_sid.is_empty() {
                script_sid.clone()
            } else {
                current_sid.clone()
            };
            mcp.resolve(&mut out, &sid, &mut |l| log.line(l));
        }
        let written = sim.is_some().then(|| out.clone());
        // With FAKE_ACP_LATE_GAP_MS the messages that follow the prompt's response come one gap apart.
        let mut after_response = false;
        for msg in out {
            if let Some(gap) = late_gap.filter(|_| after_response) {
                let _ = stdout.flush();
                std::thread::sleep(gap);
            }
            if is_prompt && msg.get("id").is_some() && msg.get("method").is_none() {
                after_response = true;
            }
            if msg
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("tool_call_update")
            {
                apply_edits(&workspace, &msg, &mut log);
            }
            let text = msg.to_string();
            log.line(&format!("out {text}"));
            let _ = writeln!(stdout, "{text}");
        }
        let _ = stdout.flush();
        if is_prompt && let (Some(s), Some(written)) = (sim.as_mut(), written) {
            let mut io = MainIo {
                stdout: &mut stdout,
                log: &mut log,
                rx: &line_rx,
                workspace: &workspace,
            };
            s.end_first_run(&current_sid, &written, &mut io);
        }
        if let Some(code) = dies.filter(|_| final_response.is_some()) {
            log.line(&format!("exiting {code} before the prompt response"));
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
        if let Some(resp) = final_response {
            log.line(&format!("silent for {silence_s} s"));
            // The silence ends at its time, or, with FAKE_ACP_CANCEL_DELAY_MS set, at a `session/cancel`, which answers the prompt
            // `cancelled` after the delay. Any other line waits for the silence to end.
            let deadline = Instant::now() + Duration::from_secs(silence_s);
            let mut text = resp.to_string();
            if cancel_delay.is_none() {
                // As before: the engine sleeps, whatever the client does (it may close stdin; agentd kills a stalled engine asleep).
                std::thread::sleep(Duration::from_secs(silence_s));
            }
            while cancel_delay.is_some() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let Ok(l) = line_rx.recv_timeout(left) else {
                    break;
                };
                if let Some(d) = cancel_delay.filter(|_| is_cancel(&l)) {
                    log.line(&format!("in {l}"));
                    log.line("notification session/cancel");
                    std::thread::sleep(d);
                    text = json!({"jsonrpc": "2.0", "id": resp.get("id").cloned(), "result": {"stopReason": "cancelled"}}).to_string();
                    break;
                }
                backlog.push_back(l);
            }
            log.line(&format!("out {text}"));
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        }
        if is_prompt
            && let Some((n, code)) = exit_after
            && cursor.prompts == n
            && !(keep_cursor && cursor.died)
        {
            cursor.died = true;
            if keep_cursor {
                cursor.save(&cursor_path);
            }
            log.line(&format!("exiting {code} after prompt {n}"));
            return ExitCode::from(code);
        }
    }
    ExitCode::SUCCESS
}
