//! Built-in configuration of the engines agentd knows by name (M2F and M2 working setups), written
//! before launch by `crate::emit::emit_engine`, and the value each engine's ACP `model` option
//! takes (`crate::emit::acp_model_value`). A profile's own `configEmitters` are rendered by
//! `crate::emit::emit_profile`; an engine with no built-in config gets none from here.

use crate::emit::{EmitContext, Emitted, pretty, write};
use serde_json::json;
use std::io;
use std::path::{Path, PathBuf};

/// The provider file the engine reads: `<data dir>/settings/providers.json`. Only this location is read.
pub fn providers_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings/providers.json")
}

/// The provider prefix of the values OpenCode's ACP `model` option takes: `<provider>/<model id>`, the provider being Amazon Bedrock.
const OPENCODE_MODEL_PREFIX: &str = "amazon-bedrock/";

/// The value an engine's ACP `model` option takes for a catalog model id.
///
/// The qualified engine takes the catalog id as it is; an engine that does not gets its arm here, keyed by name. The pin check at `session/new`,
/// the switch to another route (`session/set_config_option`) and the launch variable all ask here, so the catalog id is mapped at the ACP
/// boundary only and everything the run records (the summary, the events) keeps the catalog id ([`catalog_model_id`] reads it back).
pub fn acp_model_value(engine: &str, model: &str) -> String {
    match engine {
        "opencode" => format!("{OPENCODE_MODEL_PREFIX}{model}"),
        _ => model.to_string(),
    }
}

/// The catalog id behind a value the engine reported (the inverse of [`acp_model_value`]). A value that is not in the engine's form is returned as
/// it came, so a mismatch still shows what the engine said.
pub fn catalog_model_id(engine: &str, value: &str) -> String {
    match engine {
        "opencode" => value
            .strip_prefix(OPENCODE_MODEL_PREFIX)
            .unwrap_or(value)
            .to_string(),
        _ => value.to_string(),
    }
}

/// Built-in config for an engine agentd knows by name; other engines get none. The release line of an engine that has two is not known
/// here; [`emit_for`] takes it from the profile.
pub fn emit(ctx: &EmitContext) -> io::Result<Emitted> {
    emit_for(ctx, "")
}

/// [`emit`] for the engine release `version` of the profile (an engine whose lines read different keys follows it).
pub fn emit_for(ctx: &EmitContext, version: &str) -> io::Result<Emitted> {
    match ctx.engine.as_str() {
        "cline" => emit_cline(ctx),
        super::opencode::ENGINE => {
            super::opencode::emit(ctx, super::opencode::Line::of_version(version))
        }
        _ => Ok(Emitted::default()),
    }
}

/// The guard the prompts of `engine` pass before they are written; none for an engine that reads no prompt as a command.
pub fn prompt_guard(engine: &str) -> Option<crate::acp::PromptGuard> {
    match engine {
        super::opencode::ENGINE => Some(super::opencode::guard_prompt),
        _ => None,
    }
}

/// The engine's last word on its launch environment, after the modules added theirs (the tool server's address) and the stage's kill
/// switches are known. `template` is the profile's `[mcp]` template. An engine with nothing to add leaves the environment alone.
pub fn finalize_env(
    engine: &str,
    version: &str,
    mcp: Option<&harness_protocol::engine_profile::EngineMcp>,
    templates_root: &Path,
    disabled: &[String],
    env: &mut std::collections::BTreeMap<String, String>,
) -> io::Result<()> {
    match engine {
        super::opencode::ENGINE => {
            let template = mcp
                .and_then(|m| m.registration())
                .and_then(|r| std::fs::read_to_string(templates_root.join(r.template)).ok());
            super::opencode::finalize_env(
                super::opencode::Line::of_version(version),
                template.as_deref(),
                disabled,
                env,
            )
        }
        _ => Ok(()),
    }
}

fn emit_cline(ctx: &EmitContext) -> io::Result<Emitted> {
    let providers = providers_path(&ctx.data_dir);
    let mut settings = json!({ "provider": "bedrock", "model": ctx.model, "aws": { "authentication": "iam", "region": ctx.region } });
    if let Some(n) = ctx.window() {
        // The fork's `ProviderSettings.contextWindow` (maps to maxInputTokens): the harness names
        // the window, so a model the fork's catalog lacks (Sonnet 5.5, GPT-6.1 Sol) is not read as
        // 128K and a fork upgrade cannot change it.
        settings["contextWindow"] = json!(n);
    }
    if let Some(n) = ctx.max_output_tokens {
        // The role's output cap: without it the fork
        // sends no max-token field to Mantle, which then reserves the model maximum.
        settings["maxTokens"] = json!(n);
    }
    if let Some(block) = &ctx.compaction {
        // The user's compaction settings (protocol 1.39.0): a typed block of a bool, one of two fixed words and a number, never a user string
        // (fork patch 0012 reads `settings.compaction`). Without them the file carries no compaction key.
        settings["compaction"] = block.to_json();
    }
    write(
        &providers,
        &pretty(&json!({
            "version": 1,
            "lastUsedProvider": "bedrock",
            "modes": {},
            "providers": { "bedrock": {
                "settings": settings,
                // Cline's StoredProviderSettingsEntrySchema requires a zod datetime here; without
                // it Cline ignores the whole file with no error, maxTokens included.
                "updatedAt": crate::time::format_rfc3339(crate::time::now_unix()),
                "tokenSource": "manual",
            }},
        })),
    )?;
    let global = ctx.config_dir.join("global-settings.json");
    write(
        &global,
        &pretty(&json!({ "telemetryOptOut": true, "autoUpdateEnabled": false })),
    )?;
    let env = [
        ("CLINE_GLOBAL_SETTINGS_PATH", global.display().to_string()),
        ("CLINE_NO_AUTO_UPDATE", "1".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    Ok(Emitted {
        files: vec![providers, global],
        env,
    })
}
