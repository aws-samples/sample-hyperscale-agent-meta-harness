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

/// The value an engine's ACP `model` option takes for a catalog model id.
///
/// The qualified engine takes the catalog id as it is; an engine that does not gets its arm here, keyed by name.
pub fn acp_model_value(_engine: &str, model: &str) -> String {
    model.to_string()
}

/// Built-in config for the qualified engine; other engines get none.
pub fn emit(ctx: &EmitContext) -> io::Result<Emitted> {
    match ctx.engine.as_str() {
        "cline" => emit_cline(ctx),
        _ => Ok(Emitted::default()),
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
