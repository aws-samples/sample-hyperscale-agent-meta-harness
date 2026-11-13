//! The parity of the engines.
//! One row per protection or
//! tool, asserted for both engines that run (Cline and OpenCode) or carrying a named, justified exception. `engine/PARITY.md` holds the table
//! this file renders; a row added here without the table, or the other way round, fails.
//!
//! A row that waits on work of another lane says so (`Owed`): the table lists it, and the test `b789_parity_no_row_is_owed` fails while any is left,
//! so no row is owed on a merged tree.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::acp::{AcpClient, AcpHandler, Activity};
use agentd::direct::classify::classify;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::placement::{verify_rules_placed, verify_skills_placed};
use agentd::direct::retry::RetryPolicy;
use agentd::direct::rules_place::place_rules;
use agentd::direct::skills::place::place_skills;
use agentd::emit::{EmitContext, aws_env, emit_aws_config};
use agentd::engine::config::{acp_model_value, catalog_model_id};
use agentd::engine::direct::{rules_listing_argv, skills_listing_argv};
use agentd::engine::layout::{
    PlacementCheck, Protection, check_kill_switches, config_layout, config_root_env, kill_switches,
    lock_settings, placement_check, protect, protection_of,
};
use agentd::engine::tools::{TitleStyle, engine_tools, tool_of};
use agentd::events::EventRecorder;
use harness_core::engine_profile::{bundled_names, bundled_profile, known_engine};
use harness_protocol::api::v1::RuleFile;
use harness_protocol::direct::{DirectOutcome, Skill, SkillFile};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The engines that run. A profile the build bundles for another engine (the fake ACP engine is a test double) is no engine of the product.
const ENGINES: [&str; 2] = ["cline", "opencode"];
const MODEL: &str = "us.anthropic.claude-opus-5-5";

/// What a row says of one engine.
#[derive(Clone, Copy)]
enum Proof {
    /// The assertion runs for that engine and must pass.
    Asserted(fn(&str) -> Result<(), String>),
    /// The engine differs on purpose: the reason, which `engine/PARITY.md` repeats.
    Exception(&'static str),
    /// The work belongs to another lane of the OpenCode program and is not merged yet: (lane, what).
    #[allow(dead_code)]
    // no row is owed on the merged head; the next lane that owes one uses it
    Owed(&'static str, &'static str),
}

struct Row {
    name: &'static str,
    what: &'static str,
    cline: Proof,
    opencode: Proof,
}

impl Row {
    fn proof(&self, engine: &str) -> Proof {
        match engine {
            "cline" => self.cline,
            "opencode" => self.opencode,
            other => panic!("no column for engine {other}"),
        }
    }
}

// ---- helpers ----------------------------------------------------------------------------------------------------------------

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn repo_file(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

fn unlock(p: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(p)
        && m.is_dir()
    {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
        if let Ok(rd) = std::fs::read_dir(p) {
            rd.flatten().for_each(|e| unlock(&e.path()));
        }
    }
}

fn skill(name: &str) -> Skill {
    let text = format!(
        "---\nname: {name}\ndescription: Use when asked for {name}.\n---\n# Steps\n\n1. Do it.\n"
    );
    Skill {
        name: name.into(),
        files: vec![SkillFile {
            path: "SKILL.md".into(),
            sha256: sha(text.as_bytes()),
            size: text.len() as u64,
            content: text,
        }],
    }
}

fn profile(engine: &str) -> harness_protocol::engine_profile::EngineProfile {
    bundled_profile(known_engine(engine).unwrap())
        .unwrap()
        .unwrap()
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(f)
}

/// A layout with its root made, in a fresh folder.
fn fresh(engine: &str, label: &str) -> (agentd::engine::layout::ConfigLayout, PathBuf) {
    let base = common::scratch(label);
    let layout = config_layout(engine, &base.join("config"));
    std::fs::create_dir_all(&layout.config_dir).unwrap();
    layout.prepare().unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    (layout, repo)
}

// ---- the rows ---------------------------------------------------------------------------------------------------------------

fn rules_placement(engine: &str) -> Result<(), String> {
    let (layout, repo) = fresh(engine, "parity-rules");
    let rules = [RuleFile {
        path: "00-my-rules.md".into(),
        sha256: sha(b"Write the test first.\n"),
        content: "Write the test first.\n".into(),
    }];
    let placed = place_rules(&layout.rules, &rules)?;
    if mode(&layout.rules.join("00-my-rules.md")) != 0o444 {
        return Err("the rule is not read-only".into());
    }
    if !layout.rules.starts_with(&layout.root) {
        return Err("the rules are not under the root of the layout".into());
    }
    // Every engine is checked: by its own listing when it has one, by the files when it has none.
    let checked = match placement_check(engine) {
        PlacementCheck::Listing => rules_listing_argv(engine, "p", &layout.config_dir)
            .map(|_| ())
            .ok_or("an engine checked by its listing has no listing argv"),
        PlacementCheck::Files => verify_rules_placed(engine, &layout, &placed, &repo)
            .map_err(|_| "the files check failed")
            .and(
                rules_listing_argv(engine, "p", &layout.config_dir)
                    .is_none()
                    .then_some(())
                    .ok_or("an engine checked by files has a listing argv"),
            ),
    };
    unlock(&layout.rules);
    checked.map_err(String::from)
}

fn skills_placement(engine: &str) -> Result<(), String> {
    let (layout, repo) = fresh(engine, "parity-skills");
    let placed = place_skills(&layout.skills, &[skill("alpha")])?;
    let checked = match placement_check(engine) {
        PlacementCheck::Listing => skills_listing_argv(engine, "p", &layout.config_dir)
            .map(|_| ())
            .ok_or_else(|| "an engine checked by its listing has no skills listing".to_string()),
        PlacementCheck::Files => verify_skills_placed(engine, &layout, &placed, &repo),
    };
    unlock(&layout.skills);
    checked
}

fn config_protection(engine: &str) -> Result<(), String> {
    let (layout, _) = fresh(engine, "parity-protect");
    let own = layout.config_dir.clone();
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    if block_on(protect(&layout, None)) {
        return Err("protection acted with no engine user".into());
    }
    if !block_on(protect(&layout, Some((1234, 1234)))) {
        return Err("protection did not act for an engine user".into());
    }
    match protection_of(engine) {
        Protection::RootOwnsConfigDir if mode(&own) == 0o755 => Ok(()),
        Protection::SeparateRoot if mode(&layout.root) == 0o755 && mode(&own) == 0o700 => Ok(()),
        other => Err(format!(
            "{other:?} left the modes root {:o} config {:o}",
            mode(&layout.root),
            mode(&own)
        )),
    }
}

fn locked_settings_cline(engine: &str) -> Result<(), String> {
    let (layout, _) = fresh(engine, "parity-lock");
    std::fs::write(layout.config_dir.join("global-settings.json"), "{}").unwrap();
    let mut env = BTreeMap::new();
    let file = lock_settings(engine, &layout, &["spawn_agent".to_string()], &mut env)
        .map_err(|e| e.to_string())?
        .ok_or("no locked file")?;
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    if doc["telemetryOptOut"] != true || doc["autoUpdateEnabled"] != false {
        return Err(format!("telemetry or auto-update not off: {doc}"));
    }
    if mode(&file) != 0o444 || mode(file.parent().unwrap()) != 0o555 {
        return Err("the locked file or its directory is writable".into());
    }
    unlock(&layout.locked);
    Ok(())
}

fn config_root_variable(engine: &str) -> Result<(), String> {
    let (layout, _) = fresh(engine, "parity-root");
    let (name, value) = config_root_env(&layout);
    if name != "HAMH_ENGINE_CONFIG_ROOT" {
        return Err(format!("variable {name}"));
    }
    let root = PathBuf::from(&value);
    if layout.skills.parent() != Some(root.as_path())
        || layout.rules.parent() != Some(root.as_path())
    {
        return Err("rules/ and skills/ are not directly under the root".into());
    }
    let rules =
        std::fs::read_to_string(repo_file("crates/ham-core/assets/default-rules.md")).unwrap();
    if !rules.contains("${HAMH_ENGINE_CONFIG_ROOT}/skills/lifecycle-check/scripts/scanner_kit.py") {
        return Err("the shipped rules do not use the variable".into());
    }
    Ok(())
}

fn model_value(engine: &str) -> Result<(), String> {
    let value = acp_model_value(engine, MODEL);
    if catalog_model_id(engine, &value) != MODEL {
        return Err(format!("{value} does not map back to the catalog id"));
    }
    if value != MODEL && engine == "cline" {
        return Err("the qualified engine takes the catalog id as it is".into());
    }
    Ok(())
}

fn tool_tables(engine: &str) -> Result<(), String> {
    let t = engine_tools(engine);
    // Each engine's title style reads the titles that engine writes and no other's: Cline writes `name: args`; OpenCode titles a pending call by its tool
    // name alone and a shell call by its command once it runs, so a command is never taken for a name.
    match t.title {
        TitleStyle::NameColonArgs => {
            if tool_of(engine, "name_1: args") != "name_1" {
                return Err("the title style does not read name: args".into());
            }
        }
        TitleStyle::NameThenCommand => {
            if tool_of(engine, "name_1") != "name_1" {
                return Err("the title style does not read a tool name alone".into());
            }
            if !tool_of(engine, "cargo test --workspace").is_empty()
                || !tool_of(engine, "name_1: args").is_empty()
            {
                return Err("the title style took a command for a tool name".into());
            }
        }
    }
    // No engine is judged by the names of another.
    let other = if engine == "cline" {
        "opencode"
    } else {
        "cline"
    };
    let mine: Vec<&str> = t.known.iter().map(|k| k.name).collect();
    if engine_tools(other)
        .known
        .iter()
        .any(|k| mine.contains(&k.name))
    {
        return Err("two engines share a tool table row".into());
    }
    Ok(())
}

fn telemetry_kill_switches(engine: &str) -> Result<(), String> {
    let k = kill_switches(engine).ok_or("no kill-switch table")?;
    if k.env.is_empty() && k.locked.is_empty() {
        return Err("nothing is required of the launch".into());
    }
    let p = profile(engine);
    for (var, value) in k.env {
        if p.env.get(*var).map(String::as_str) != Some(*value) {
            return Err(format!("the profile's [env] lacks {var}={value}"));
        }
    }
    // A launch without them is refused, with them accepted (the locked part is the row above).
    let mut env: BTreeMap<String, String> = k
        .env
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    if k.locked.is_empty() {
        check_kill_switches(engine, &env, None)?;
    }
    if let Some((var, _)) = k.env.first() {
        env.remove(*var);
        if check_kill_switches(engine, &env, None).is_ok() {
            return Err(format!("a launch without {var} was accepted"));
        }
    }
    Ok(())
}

fn credentials_binding(engine: &str) -> Result<(), String> {
    let base = common::scratch("parity-creds");
    let ctx = EmitContext {
        engine: engine.into(),
        model: MODEL.into(),
        region: "us-west-2".into(),
        data_dir: base.join("data"),
        config_dir: base.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: base.join("creds.sock"),
        max_output_tokens: None,
        context_tokens: None,
        compaction: None,
    };
    let cfg = emit_aws_config(&ctx).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&cfg).unwrap();
    if !text.contains("[profile agent]")
        || !text.contains("credential_process = /opt/harness/bin/agentd creds --socket")
    {
        return Err(format!("the profile is not bound to agentd creds: {text}"));
    }
    let env = aws_env(&ctx, &cfg);
    if env.get("AWS_PROFILE").map(String::as_str) != Some("agent")
        || env.get("AWS_EC2_METADATA_DISABLED").map(String::as_str) != Some("true")
    {
        return Err("the launch is not bound to the agent profile".into());
    }
    if env.keys().any(|k| {
        k == "AWS_ACCESS_KEY_ID"
            || k == "AWS_SECRET_ACCESS_KEY"
            || k == "AWS_SESSION_TOKEN"
            || k == "AWS_BEARER_TOKEN_BEDROCK"
    }) {
        return Err("a raw key or token is in the launch environment".into());
    }
    if !profile(engine)
        .env_remove
        .iter()
        .any(|v| v == "AWS_BEARER_TOKEN_BEDROCK")
    {
        return Err("the profile does not remove the Bedrock bearer token".into());
    }
    Ok(())
}

fn hidden_paths(engine: &str) -> Result<(), String> {
    let p = profile(engine);
    if p.hidden_paths.is_empty() {
        return Err("no hidden path".into());
    }
    if p.hidden_paths
        .iter()
        .any(|h| h.starts_with('/') || h.contains(".."))
    {
        return Err("a hidden path leaves the repository".into());
    }
    let own = match engine {
        "cline" => ".cline/hooks/**",
        "opencode" => "opencode.json",
        _ => return Err("no engine file named".into()),
    };
    if !p.hidden_paths.iter().any(|h| h == own) {
        return Err(format!("{own} is not hidden"));
    }
    // OpenCode reads its project configuration from either spelling and from its dot folder.
    if engine == "opencode" {
        for also in ["opencode.jsonc", ".opencode/**"] {
            if !p.hidden_paths.iter().any(|h| h == also) {
                return Err(format!("{also} is not hidden"));
            }
        }
    }
    Ok(())
}

/// The session document and launch additions the OpenCode arm writes for the release line of the bundled profile.
fn opencode_emitted(label: &str) -> (EmitContext, agentd::emit::Emitted) {
    let base = common::scratch(label);
    let ctx = EmitContext {
        engine: "opencode".into(),
        model: MODEL.into(),
        region: "us-west-2".into(),
        data_dir: base.join("data"),
        config_dir: base.join("config"),
        agentd_exe: PathBuf::from("/opt/harness/bin/agentd"),
        socket: PathBuf::from("/run/agentd/creds.sock"),
        max_output_tokens: Some(64_000),
        context_tokens: Some(1_000_000),
        compaction: None,
    };
    let out = agentd::emit::emit_engine_for(&ctx, &profile("opencode").version).unwrap();
    (ctx, out)
}

fn rules_reach_opencode(_engine: &str) -> Result<(), String> {
    let (ctx, out) = opencode_emitted("parity-oc-rules");
    let layout = config_layout("opencode", &ctx.config_dir);
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&out.files[0]).unwrap()).unwrap();
    let want = json!([format!("{}/*.md", layout.rules.display())]);
    if doc["instructions"] != want {
        return Err(format!(
            "instructions are {}, not {want}",
            doc["instructions"]
        ));
    }
    Ok(())
}

fn locked_settings_opencode(_engine: &str) -> Result<(), String> {
    let (ctx, out) = opencode_emitted("parity-oc-lock");
    let layout = config_layout("opencode", &ctx.config_dir);
    let file = layout.root.join("opencode.json");
    if protection_of("opencode") != Protection::SeparateRoot
        || file.starts_with(&layout.config_dir)
        || out.files != vec![file.clone()]
    {
        return Err(format!(
            "the document is not in the harness's own root: {:?}",
            out.files
        ));
    }
    if out.env.get("OPENCODE_CONFIG").map(String::as_str) != Some(&*file.display().to_string()) {
        return Err("OPENCODE_CONFIG does not name the file in the root".into());
    }
    // The inline layer outranks every file the engine can write and holds the same document.
    let inline: Value = serde_json::from_str(
        out.env
            .get("OPENCODE_CONFIG_CONTENT")
            .ok_or("no inline layer")?,
    )
    .unwrap();
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    if inline != on_disk
        || inline["share"] != "disabled"
        || inline["permission"]["doom_loop"] == "ask"
    {
        return Err("the inline layer differs from the file or does not lock the settings".into());
    }
    Ok(())
}

fn delivery_refusal_opencode(engine: &str) -> Result<(), String> {
    hidden_paths(engine)?;
    // The validator is a crate of its own that this one does not depend on; its source names both spellings in both modes, and its own test
    // (crates/delivery/tests/b790_engine_config_files.rs) runs them.
    let src = std::fs::read_to_string(repo_file("crates/delivery/src/validator.rs")).unwrap();
    if src.matches("\"opencode.jsonc\"").count() < 2 {
        return Err("the validator does not refuse opencode.jsonc in both modes".into());
    }
    Ok(())
}

fn tool_server_opencode(engine: &str) -> Result<(), String> {
    tool_server_cline(engine)?;
    let template =
        std::fs::read_to_string(repo_file("engine/profiles/templates/mcp-opencode.json")).unwrap();
    let (_, out) = opencode_emitted("parity-oc-tools");
    let mut env = out.env.clone();
    env.insert("HAMH_TOOLS_URL".into(), "http://127.0.0.1:4/mcp".into());
    let line = agentd::engine::opencode::Line::of_version(&profile(engine).version);
    agentd::engine::opencode::finalize_env(line, Some(&template), &[], &mut env)
        .map_err(|e| e.to_string())?;
    let inline: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    if !inline.to_string().contains("http://127.0.0.1:4/mcp") || inline["mcp"].is_null() {
        return Err(format!(
            "the tool server is not in the inline layer: {}",
            inline["mcp"]
        ));
    }
    Ok(())
}

fn tool_idle_limit(engine: &str) -> Result<(), String> {
    use agentd::engine::watchdog::tool_idle;
    let default = tool_idle(engine, None);
    let want = match engine {
        "cline" => None,
        "opencode" => Some(Duration::from_secs(3600)),
        other => return Err(format!("no default named for {other}")),
    };
    if default != want {
        return Err(format!("default {default:?}, expected {want:?}"));
    }
    if tool_idle(engine, Some(900)) != Some(Duration::from_secs(900))
        || tool_idle(engine, Some(0)).is_some()
    {
        return Err("the setting does not replace the default or does not switch it off".into());
    }
    Ok(())
}

fn slash_prompt(engine: &str) -> Result<(), String> {
    let guard = agentd::engine::config::prompt_guard(engine);
    match engine {
        "cline" if guard.is_none() => Ok(()),
        "opencode" => {
            let g = guard.ok_or("no guard")?;
            for word in ["/goal", "/init", "/review", "/compact"] {
                let task = format!("{word} do it");
                let wire = g(&task);
                if wire.trim_start().starts_with('/') || !wire.ends_with(&task) {
                    return Err(format!("{word} still starts the prompt: {wire:?}"));
                }
            }
            if g("plain task") != "plain task" {
                return Err("a plain task was changed".into());
            }
            Ok(())
        }
        other => Err(format!("{other}: unexpected guard {}", guard.is_some())),
    }
}

fn permission_answering(engine: &str) -> Result<(), String> {
    let rec = EventRecorder::new("run1", "direct");
    let mut c = DirectCollector::new(rec).with_engine(engine);
    let params = json!({
        "sessionId": "s1",
        "toolCall": {"toolCallId": "t9", "title": "edit: a.txt"},
        "options": [
            {"kind": "reject_once", "optionId": "reject"},
            {"kind": "allow_always", "optionId": "always"},
            {"kind": "allow_once", "optionId": "once"},
        ],
    });
    let result = c
        .on_request("session/request_permission", &params)
        .map_err(|e| format!("{e:?}"))?;
    if result["outcome"]["outcome"] != "selected" || result["outcome"]["optionId"] != "once" {
        return Err(format!("not allowed once: {result}"));
    }
    Ok(())
}

type Client = AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream>;
type Wire = Arc<Mutex<Vec<String>>>;

/// An engine that reports `value` as its model, then goes quiet after one chunk of text.
fn quiet_engine(value: String) -> (Client, Wire) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let wire: Wire = Arc::default();
    let seen = wire.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            seen.lock()
                .unwrap()
                .push(msg["method"].as_str().unwrap_or_default().to_string());
            let _ = tx.send(msg);
        }
    });
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| {
                format!(
                    "{}\n",
                    json!({"jsonrpc": "2.0", "id": id, "result": result})
                )
            };
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => out.write_all(reply(json!({"protocolVersion": 1, "agentInfo": {"name": "scripted", "version": "9.9.9"}})).as_bytes()).await.unwrap(),
                "session/new" => out.write_all(reply(json!({"sessionId": "s-1", "configOptions": [{"id": "model", "currentValue": value, "options": []}]})).as_bytes()).await.unwrap(),
                "session/prompt" => {
                    let chunk = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1",
                        "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "thinking"}}}});
                    out.write_all(format!("{chunk}\n").as_bytes()).await.unwrap();
                    out.flush().await.unwrap();
                    std::future::pending::<()>().await;
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    (AcpClient::new(client_r, client_w), wire)
}

fn idle_watchdog(engine: &str) -> Result<(), String> {
    block_on(async {
        let (client, wire) = quiet_engine(acp_model_value(engine, MODEL));
        let activity = Activity::new();
        let mut client = client.with_activity(activity.clone());
        let mut collector =
            DirectCollector::new(EventRecorder::new("run1", "direct")).with_engine(engine);
        let plan = PromptPlan {
            cwd: "/mnt/workspace/run1/direct/repo".into(),
            engine: engine.into(),
            model: MODEL.into(),
            prompt: "go".into(),
            nudge: None,
            cancel: None,
            timeout: Duration::from_secs(27_900),
            silence: Duration::from_secs(900),
            retry: RetryPolicy::off(),
            failover: None,
            engine_stop_resumes: 0,
        };
        let t0 = tokio::time::Instant::now();
        let end = run_direct_prompt(&mut client, &mut collector, &activity, &plan, &NoProbe).await;
        let took = t0.elapsed().as_secs();
        let outcome = classify(&end.raw).outcome;
        tokio::task::yield_now().await;
        if outcome != DirectOutcome::StoppedSilence || !(900..915).contains(&took) {
            return Err(format!("{outcome:?} after {took} s"));
        }
        if !wire.lock().unwrap().iter().any(|m| m == "session/cancel") {
            return Err("no session/cancel was written before the engine was given up on".into());
        }
        Ok(())
    })
}

fn tool_server_cline(engine: &str) -> Result<(), String> {
    let p = profile(engine);
    let mcp = p.mcp.as_ref().ok_or("the profile has no [mcp] table")?;
    let template = mcp
        .template
        .as_deref()
        .ok_or("the [mcp] table names no template")?;
    if !repo_file(template).is_file() {
        return Err(format!("template {template} is missing"));
    }
    Ok(())
}

fn rg_in_the_shipped_rules(_engine: &str) -> Result<(), String> {
    let rules =
        std::fs::read_to_string(repo_file("crates/ham-core/assets/default-rules.md")).unwrap();
    let line = rules
        .lines()
        .find(|l| l.contains("Search the repository with `rg`"))
        .ok_or("no ripgrep rule")?;
    if line.to_ascii_lowercase().contains("cline") || line.to_ascii_lowercase().contains("opencode")
    {
        return Err("the ripgrep rule names an engine".into());
    }
    Ok(())
}

const GATEWAY: &str = "The harness gives no AgentCore MCP gateway tool to any engine today. A gateway tool added later reaches every engine through the harness tool server of the row above.";
const SEMANTIC: &str = "The harness gives no semantic search to any engine today; the code_search tool of the harness's tool server is a ripgrep search of the work tree, the same for every engine.";
const AWS_API: &str = "The harness gives no AWS API MCP to any engine today; an engine reaches AWS with the session credentials of the credentials row, through its own tools.";

// ---- the proofs by effect ------------------------------------------------------------------------------------------- The harness-side rows
// above say what the launch holds. These rows say that the engine's profile gates it on the proofs that run the engine and look at its effects,
// and that the hosts the firewall drops are written alike.

const DROPPED_NAMES: [&str; 4] = [
    "registry.npmjs.org",
    "models.dev",
    "opencode.ai",
    "github.com",
];

fn repo_text(rel: &str) -> Result<String, String> {
    std::fs::read_to_string(repo_file(rel)).map_err(|e| format!("{rel}: {e}"))
}

/// The model that needs provider data sharing and runs on every engine that lists it.
const SHARING_MODEL: &str = "global.anthropic.claude-fable-5-2";

/// The agreement to provider data sharing is about the model and its provider, not about the engine: `ham` and the service refuse a run that did not agree
/// before any call, whichever engine it names. The engine's rows in `models.json` carry the mark the refusal reads, so the model has a row for the engine and
/// the row carries the mark, and no row of any engine disagrees with the other rows of its model.
fn provider_sharing_agreement(engine: &str) -> Result<(), String> {
    let doc: Value = serde_json::from_str(&repo_text("engine/catalog/models.json")?)
        .map_err(|e| format!("models.json: {e}"))?;
    let rows = doc["models"]
        .as_array()
        .ok_or("models.json has no models")?;
    let engine_of = |r: &Value| {
        r.get("engine")
            .and_then(Value::as_str)
            .unwrap_or("cline")
            .to_string()
    };
    let marked = |r: &Value| {
        r.get("requiresProviderSharing")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let mut listed = false;
    for row in rows.iter().filter(|r| engine_of(r) == engine) {
        let id = row["id"].as_str().unwrap_or_default();
        for twin in rows.iter().filter(|t| t["id"] == row["id"]) {
            if marked(twin) != marked(row) {
                return Err(format!(
                    "{id}: needs sharing is {} on {} and {} on {engine}",
                    marked(twin),
                    engine_of(twin),
                    marked(row)
                ));
            }
        }
        if id == SHARING_MODEL {
            listed = true;
            if !marked(row) {
                return Err(format!("the {engine} row of {id} lacks the mark"));
            }
        }
    }
    if listed {
        Ok(())
    } else {
        Err(format!("{SHARING_MODEL} has no row for {engine}"))
    }
}

/// The engine's profile carries a gating row `K<number>` (or `K<number>-<letter>`): the row that runs the proof at qualification.
fn gating_row(engine: &str, number: &str) -> Result<(), String> {
    let known = bundled_names();
    if !known.contains(&engine) {
        return Err(format!("{engine} has no bundled profile"));
    }
    let p = profile(engine);
    let row = p
        .rows
        .iter()
        .find(|r| r.id == number || r.id.starts_with(&format!("{number}-")))
        .ok_or_else(|| format!("the profile of {engine} has no row {number}"))?;
    if row.class == harness_protocol::engine_profile::RowClass::Gating {
        Ok(())
    } else {
        Err(format!("row {} of {engine} does not gate", row.id))
    }
}

fn egress_proof(engine: &str) -> Result<(), String> {
    gating_row(engine, "K10")
}

fn credentials_proof(engine: &str) -> Result<(), String> {
    gating_row(engine, "K13")
}

fn egress_allowlist(engine: &str) -> Result<(), String> {
    let network = repo_text("infra/lib/stacks/network.ts")?;
    for name in DROPPED_NAMES {
        if !network.contains(name) {
            return Err(format!("network.ts does not name the dropped host {name}"));
        }
    }
    for (var, value) in &profile(engine).env {
        if let Some(name) = DROPPED_NAMES.iter().find(|n| value.contains(**n)) {
            return Err(format!(
                "the profile sets {var} to a value naming {name}, which the firewall drops"
            ));
        }
    }
    Ok(())
}

/// The shell command time limit: one setting, `commands.timeout_seconds`, with one range and one default in the engine's key table; the
/// user's value reaches the engine's own variable in milliseconds; with no setting the engine ends up with ten minutes (OpenCode is given them in
/// every launch because its own default is two, Cline's fork has them as its own default); a launch without the variable is refused.
fn command_time_limit(engine: &str) -> Result<(), String> {
    use agentd::engine::direct::launch_env;
    use agentd::engine::user_settings::{UserSettings, check_command_limit};
    use harness_protocol::direct::{EngineConfig, KeyKind, check_engine_config, keys_for};
    let (var, placed_by_default) = match engine {
        "cline" => ("CLINE_RUN_COMMANDS_TIMEOUT_MS", false),
        "opencode" => ("OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS", true),
        other => return Err(format!("no variable named for {other}")),
    };
    let row = keys_for(engine)
        .and_then(|rows| rows.iter().find(|r| r.path == "commands.timeout_seconds"))
        .ok_or("the key table has no commands.timeout_seconds")?;
    if row.kind != (KeyKind::Int { min: 1, max: 1800 }) || row.default != "600" {
        return Err(format!(
            "the key is {:?}, default {}",
            row.kind, row.default
        ));
    }
    let member = EngineConfig {
        engine: engine.into(),
        schema: 1,
        settings: json!({"commands": {"timeout_seconds": 900}})
            .as_object()
            .unwrap()
            .clone(),
    };
    check_engine_config(&member).map_err(|e| e.to_string())?;
    let set = UserSettings::from_checked(&member)?;
    let env = launch_env(engine, &set);
    if env.len() != 1 || env.get(var).map(String::as_str) != Some("900000") {
        return Err(format!(
            "a limit of 900 s is placed as {env:?}, not {var}=900000"
        ));
    }
    let none = launch_env(engine, &UserSettings::default());
    if placed_by_default {
        if none.len() != 1 || none.get(var).map(String::as_str) != Some("600000") {
            return Err(format!(
                "no setting is placed as {none:?}, not {var}=600000"
            ));
        }
    } else {
        let patches = repo_file("engine/cline/patches");
        let patch = std::fs::read_dir(&patches)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("0015-"))
            })
            .ok_or("patch 0015 is missing")?;
        let text = std::fs::read_to_string(patch).map_err(|e| e.to_string())?;
        if !none.is_empty() || !text.contains("a 600000 ms default") {
            return Err(format!(
                "no setting adds {none:?}, or the fork's own default is not 600000 ms"
            ));
        }
    }
    if check_command_limit(engine, &BTreeMap::new(), &set).is_ok() {
        return Err("a launch without the variable is accepted".into());
    }
    Ok(())
}

fn rows() -> Vec<Row> {
    // `Owed` stays imported for the next lane that owes a row.
    #[allow(unused_imports)]
    use Proof::{Asserted as A, Exception as E, Owed as O};
    vec![
        Row {
            name: "rules placement",
            what: "the user's rules are placed read-only under the root of the layout and checked before any model call",
            cline: A(rules_placement),
            opencode: A(rules_placement),
        },
        Row {
            name: "skills placement",
            what: "the run's skills are placed read-only under the root of the layout and checked before any model call",
            cline: A(skills_placement),
            opencode: A(skills_placement),
        },
        Row {
            name: "rules reach the engine",
            what: "the engine is told to load the placed rules",
            cline: A(rules_placement),
            opencode: A(rules_reach_opencode),
        },
        Row {
            name: "config protection",
            what: "the engine user cannot rewrite what the harness placed",
            cline: A(config_protection),
            opencode: A(config_protection),
        },
        Row {
            name: "locked settings",
            what: "settings the engine user cannot rewrite",
            cline: A(locked_settings_cline),
            opencode: A(locked_settings_opencode),
        },
        Row {
            name: "config root variable",
            what: "HAMH_ENGINE_CONFIG_ROOT names the root of the harness's files in every launch",
            cline: A(config_root_variable),
            opencode: A(config_root_variable),
        },
        Row {
            name: "model value on the ACP wire",
            what: "the catalog id is mapped to the engine's own value at the ACP boundary only",
            cline: A(model_value),
            opencode: A(model_value),
        },
        Row {
            name: "tool and title tables",
            what: "each engine's tool names and title style live in one table; none is judged by another's",
            cline: A(tool_tables),
            opencode: A(tool_tables),
        },
        Row {
            name: "permission answering",
            what: "every permission request is answered at once, allow once",
            cline: A(permission_answering),
            opencode: A(permission_answering),
        },
        Row {
            name: "idle watchdog",
            what: "an engine silent past the limit is cancelled and given up on",
            cline: A(idle_watchdog),
            opencode: A(idle_watchdog),
        },
        Row {
            name: "tool-idle limit",
            what: "a tool call that runs while the engine sends nothing is cancelled after the limit; each engine starts with its own default, one setting replaces it",
            cline: A(tool_idle_limit),
            opencode: A(tool_idle_limit),
        },
        Row {
            name: "command time limit",
            what: "the shell command time limit of the engine is one setting, placed in the launch, and is ten minutes when the user sets none",
            cline: A(command_time_limit),
            opencode: A(command_time_limit),
        },
        Row {
            name: "task text that starts with a slash",
            what: "a task that starts with / reaches the model as a task, and an engine that reads no prompt as a command gets its prompts byte for byte",
            cline: A(slash_prompt),
            opencode: A(slash_prompt),
        },
        Row {
            name: "telemetry kill switches",
            what: "telemetry and self-update are off in every launch, and a launch without them is refused",
            cline: A(telemetry_kill_switches),
            opencode: A(telemetry_kill_switches),
        },
        Row {
            name: "credentials binding",
            what: "the engine reaches AWS through the agent profile and agentd creds, with no raw key",
            cline: A(credentials_binding),
            opencode: A(credentials_binding),
        },
        Row {
            name: "telemetry proof by effect",
            what: "the engine's profile gates it on an egress capture of a full turn: no name outside the allowlist",
            cline: A(egress_proof),
            opencode: A(egress_proof),
        },
        Row {
            name: "credential proof by effect",
            what: "the engine's profile gates it on credential binding: every call is signed by the session profile and a stopped broker fails closed",
            cline: A(credentials_proof),
            opencode: A(credentials_proof),
        },
        Row {
            name: "egress allowlist",
            what: "the network source names the hosts the firewall drops, and no engine profile points at one",
            cline: A(egress_allowlist),
            opencode: A(egress_allowlist),
        },
        Row {
            name: "hidden paths",
            what: "the engine's own config files are hidden from the work (the hiddenPaths of its profile)",
            cline: A(hidden_paths),
            opencode: A(hidden_paths),
        },
        Row {
            name: "delivery refusal of the config file",
            what: "the delivery validator refuses every config file of the engine",
            cline: A(hidden_paths),
            opencode: A(delivery_refusal_opencode),
        },
        Row {
            name: "harness tool server (code_search)",
            what: "the harness's tool server is registered with the engine",
            cline: A(tool_server_cline),
            opencode: A(tool_server_opencode),
        },
        Row {
            name: "ripgrep",
            what: "the engine is told to search with rg",
            cline: A(rg_in_the_shipped_rules),
            opencode: A(rg_in_the_shipped_rules),
        },
        Row {
            name: "provider data sharing agreement",
            what: "a model that needs provider data sharing carries the mark on the rows of every engine that lists it, so a run that did not agree is refused before any call",
            cline: A(provider_sharing_agreement),
            opencode: A(provider_sharing_agreement),
        },
        Row {
            name: "AgentCore MCP gateway tools",
            what: "tools of the AgentCore MCP gateway",
            cline: E(GATEWAY),
            opencode: E(GATEWAY),
        },
        Row {
            name: "semantic-engine access",
            what: "semantic search for the engine",
            cline: E(SEMANTIC),
            opencode: E(SEMANTIC),
        },
        Row {
            name: "AWS API MCP access",
            what: "an AWS API MCP for the engine",
            cline: E(AWS_API),
            opencode: E(AWS_API),
        },
    ]
}

// ---- the table --------------------------------------------------------------------------------------------------------------

const BEGIN: &str = "<!-- parity:begin -->";
const END: &str = "<!-- parity:end -->";

fn word(p: Proof) -> &'static str {
    match p {
        Proof::Asserted(_) => "asserted",
        Proof::Exception(_) => "exception",
        Proof::Owed(..) => "owed",
    }
}

/// The table, the exceptions and the owed rows, as `engine/PARITY.md` carries them between its markers.
fn render() -> String {
    let rows = rows();
    let mut out = String::from("| Row | What it covers | Cline | OpenCode |\n|---|---|---|---|\n");
    for r in &rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            r.name,
            r.what,
            word(r.cline),
            word(r.opencode)
        ));
    }
    out.push_str("\n### Exceptions\n\n");
    let mut any = false;
    for r in &rows {
        match (r.cline, r.opencode) {
            (Proof::Exception(a), Proof::Exception(b)) if a == b => {
                any = true;
                out.push_str(&format!("- {} (both engines): {}\n", r.name, a));
            }
            _ => {
                for e in ENGINES {
                    if let Proof::Exception(why) = r.proof(e) {
                        any = true;
                        out.push_str(&format!("- {} ({}): {}\n", r.name, e, why));
                    }
                }
            }
        }
    }
    if !any {
        out.push_str("None.\n");
    }
    out.push_str("\n### Owed by another lane of the OpenCode program\n\n");
    let mut any = false;
    for r in &rows {
        for e in ENGINES {
            if let Proof::Owed(lane, what) = r.proof(e) {
                any = true;
                out.push_str(&format!("- {} ({}): {}, {}\n", r.name, e, lane, what));
            }
        }
    }
    if !any {
        out.push_str("None.\n");
    }
    out
}

fn doc_block() -> (PathBuf, String) {
    let path = repo_file("engine/PARITY.md");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let block = text
        .split_once(BEGIN)
        .and_then(|(_, rest)| rest.split_once(END))
        .map(|(b, _)| b.trim_start_matches('\n').to_string())
        .unwrap_or_default();
    (path, block)
}

// ---- the tests --------------------------------------------------------------------------------------------------------------

#[test]
fn b789_parity_the_engines_of_the_table_are_the_engines_the_build_has_profiles_for() {
    let mut profiles: Vec<&str> = bundled_names()
        .into_iter()
        .filter(|n| *n != "fake-acp")
        .collect();
    profiles.sort_unstable();
    let mut table = ENGINES.to_vec();
    table.sort_unstable();
    assert_eq!(
        profiles, table,
        "a new engine profile needs a column in this table (and a row of PARITY.md) before it ships"
    );
}

#[test]
fn b789_parity_every_row_is_asserted_for_both_engines_or_carries_a_named_exception() {
    let mut failures = Vec::new();
    for r in rows() {
        for e in ENGINES {
            match r.proof(e) {
                Proof::Asserted(f) => {
                    if let Err(why) = f(e) {
                        failures.push(format!("{} / {e}: {why}", r.name));
                    }
                }
                Proof::Exception(why) => {
                    if why.len() < 40 || !why.ends_with('.') {
                        failures.push(format!(
                            "{} / {e}: the exception has no real reason: {why:?}",
                            r.name
                        ));
                    }
                }
                Proof::Owed(lane, what) => {
                    if !["oc-arm", "oc-goal", "oc-proofs", "oc-wire", "oc-qual"].contains(&lane)
                        || what.len() < 10
                    {
                        failures.push(format!("{} / {e}: owed to nobody: {lane} {what:?}", r.name));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn b789_parity_the_table_lists_every_protection_and_tool_the_owner_named() {
    let names: Vec<&str> = rows().iter().map(|r| r.name).collect();
    for want in [
        "rules placement",
        "skills placement",
        "config protection",
        "locked settings",
        "permission answering",
        "idle watchdog",
        "telemetry kill switches",
        "credentials binding",
        "hidden paths",
        "delivery refusal of the config file",
        "harness tool server (code_search)",
        "AgentCore MCP gateway tools",
        "ripgrep",
        "provider data sharing agreement",
        "semantic-engine access",
        "AWS API MCP access",
    ] {
        assert!(names.contains(&want), "row {want:?} is missing: {names:?}");
    }
    let mut seen = std::collections::BTreeSet::new();
    assert!(
        names.iter().all(|n| seen.insert(*n)),
        "a row name is used twice"
    );
}

#[test]
fn b789_parity_engine_parity_md_carries_exactly_the_table_of_this_file() {
    let (path, block) = doc_block();
    let want = render();
    if std::env::var("PARITY_WRITE").is_ok() {
        let text = std::fs::read_to_string(&path).unwrap();
        let (head, rest) = text.split_once(BEGIN).unwrap();
        let (_, tail) = rest.split_once(END).unwrap();
        std::fs::write(&path, format!("{head}{BEGIN}\n{want}{END}{tail}")).unwrap();
        return;
    }
    assert_eq!(
        block, want,
        "engine/PARITY.md between {BEGIN} and {END} is not the table of tests/engine_parity.rs; rerun with PARITY_WRITE=1 to rewrite it"
    );
}

#[test]
fn b789_parity_no_row_is_owed() {
    let owed: Vec<String> = rows()
        .iter()
        .flat_map(|r| {
            ENGINES.iter().filter_map(move |e| match r.proof(e) {
                Proof::Owed(lane, what) => Some(format!("{} ({e}): {lane}, {what}", r.name)),
                _ => None,
            })
        })
        .collect();
    assert!(owed.is_empty(), "rows still owed:\n{}", owed.join("\n"));
}

#[test]
fn b800_parity_the_command_time_limit_is_a_row_of_the_table_asserted_for_both_engines() {
    let row = rows()
        .into_iter()
        .find(|r| r.name == "command time limit")
        .expect("the row \"command time limit\" is missing");
    for engine in ENGINES {
        match row.proof(engine) {
            Proof::Asserted(f) => f(engine).unwrap_or_else(|e| panic!("{engine}: {e}")),
            _ => panic!("the command time limit is not asserted for {engine}"),
        }
    }
    // The assertion can fail: an engine it has no variable for, and a table without the key, are refused by name.
    assert!(command_time_limit("pi").unwrap_err().contains("pi"));
}

#[test]
fn b793_parity_the_proofs_by_effect_are_rows_of_the_table_for_both_engines() {
    for name in [
        "telemetry proof by effect",
        "credential proof by effect",
        "egress allowlist",
    ] {
        let row = rows()
            .into_iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("row {name:?} is missing"));
        for engine in ENGINES {
            match row.proof(engine) {
                Proof::Asserted(f) => {
                    f(engine).unwrap_or_else(|e| panic!("{name} ({engine}): {e}"))
                }
                _ => panic!("{name} is not asserted for {engine}"),
            }
        }
    }
}

#[test]
fn b793_parity_an_engine_or_a_row_the_profiles_do_not_have_is_refused_by_name() {
    // an engine added to the build without its qualification rows must fail these rows, not pass them
    assert!(egress_proof("pi").unwrap_err().contains("pi"));
    assert!(credentials_proof("pi").unwrap_err().contains("pi"));
    assert!(gating_row("cline", "K99").unwrap_err().contains("K99"));
}

#[test]
fn b793_parity_the_dropped_hosts_are_in_the_network_source_and_in_no_profile() {
    assert_eq!(egress_allowlist("cline"), Ok(()));
    assert_eq!(egress_allowlist("opencode"), Ok(()));
}
