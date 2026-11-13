//! Module `engine_config`: the user's settings for the engine, from the `engineConfig`
//! member of the start request. The driver applies them where it writes the engine's configuration (`direct/mod.rs` startup steps 7 and 9:
//! the compaction block of the provider file, the `--thinking` argument, the command time limit variable and the union of the tools switched off,
//! all through `crate::engine::user_settings`, which holds the engine's words). This module is the seam's part: it proves the placement, tells the
//! card, and fills the summary.
//!
//! `on_startup`, after the driver wrote everything and the configuration directory became root's:
//!
//! 0. An engine whose launch always carries a command time limit has that variable checked first, with a member or
//!    without: a launch without it ends the run the way a read-back mismatch does. For the engine that carries it only when the user set it, this
//!    step looks at nothing.
//! 1. A request with no member, or a member that sets no key, stops here: nothing is read, posted or logged, so a run without engine settings is
//!    the run it was (the 13 golden files do not move).
//! 2. The member passes the shared check again ([`crate::direct::request::engine_settings`]); a refusal ends the run `stopped_setup_failed`
//!    ("did not start") with the key and the rule, never the value. (The driver checked it at the top of the startup already; a module registered
//!    alone is as strict.)
//! 3. [`crate::engine::user_settings::read_back`] reads the argument, the variable and the two files again and compares them with what the
//!    settings call for. A mismatch ends the run the same way, before the first model call.
//! 4. One `turn.progress` event of kind `engine-config` names the keys that were placed (key and value as short texts, at most 24, inside the
//!    4,096-byte inline limit) with the settings hash and `readBack: "ok"`, and one log line says how many keys were placed. The event is posted
//!    only after the read-back compared equal. "Placed" means agentd wrote the value where the engine reads it; it does not mean the engine
//!    confirmed it.
//!
//! `on_summary` fills `DirectSummary.engineConfig` (engine, schema, the settings hash and the key count, never a value) when step 4 happened.
//! The module votes on nothing and the user's prompt is not changed (root invariant 1).

use crate::BoxFuture;
use crate::direct::hooks::{DirectHook, HookEnv, StartupCtx, SummaryCtx};
use crate::direct::request::engine_settings;
use crate::engine::user_settings::{Placed, check_command_limit, read_back};
use harness_protocol::direct::{DIRECT_TURN, DirectEngineConfigRef, EngineConfig, keys_for};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// The most settings the event lists.
const MAX_LISTED: usize = 24;
/// The longest value the event shows, in characters.
const MAX_VALUE_CHARS: usize = 64;
/// The inline limit of an event payload, in bytes.
const MAX_EVENT_BYTES: usize = 4096;

/// The module, always registered: it acts only when the request carries a member that sets at least one key. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    Some(Arc::new(EngineConfigHook::new()))
}

/// The module.
#[derive(Debug, Default)]
pub struct EngineConfigHook {
    placed: Mutex<Option<DirectEngineConfigRef>>,
}

impl EngineConfigHook {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `summary.json` records; `None` until the settings were placed and read back.
    pub fn placed(&self) -> Option<DirectEngineConfigRef> {
        self.placed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn startup(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let settings = engine_settings(ctx.request, &ctx.env.engine)?;
        check_command_limit(&ctx.env.engine, &*ctx.engine_env, &settings)?;
        let Some(cfg) = &ctx.request.engine_config else {
            return Ok(());
        };
        if cfg.key_count() == 0 {
            return Ok(());
        }
        read_back(
            &Placed {
                engine: &ctx.env.engine,
                config_dir: ctx.config_dir,
                data_dir: ctx.data_dir,
                argv: ctx.engine_argv(),
                env: &*ctx.engine_env,
                stage_disabled: &ctx.env.cfg.direct.disabled_tools,
            },
            &settings,
        )?;
        let reference = cfg.to_ref();
        ctx.env.log.line(&format!(
            "direct engine settings placed: {} key(s), read back ok",
            reference.keys
        ));
        ctx.env
            .rec
            .record(EventType::TurnProgress, event(cfg, &reference));
        *self
            .placed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reference);
        Ok(())
    }
}

impl DirectHook for EngineConfigHook {
    fn name(&self) -> &'static str {
        "engine_config"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.startup(ctx))
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            ctx.summary.engine_config = self.placed();
        })
    }
}

/// A value as the card shows it: a text as it is, a list as its members joined by commas, a number as written; at most 64 characters.
fn value_text(v: &Value) -> String {
    let text = match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    };
    text.chars().take(MAX_VALUE_CHARS).collect()
}

/// The `turn.progress` event of kind `engine-config`: the keys that were placed, in the key table's order, each with its value as a short text. The
/// names are the table's own and the values come from closed sets and numbers, so the event holds nothing of the user's file but those. Entries are
/// dropped from the end until the payload fits the inline limit.
pub fn event(cfg: &EngineConfig, reference: &DirectEngineConfigRef) -> Value {
    let mut settings: Vec<Value> = keys_for(&cfg.engine)
        .unwrap_or(&[])
        .iter()
        .filter_map(|row| {
            cfg.get(row.path)
                .map(|v| json!({"key": row.path, "value": value_text(v)}))
        })
        .take(MAX_LISTED)
        .collect();
    loop {
        let ev = json!({
            "turn": DIRECT_TURN,
            "kind": "engine-config",
            "engine": reference.engine,
            "sha256": reference.sha256,
            "keys": reference.keys,
            "settings": settings,
            "readBack": "ok",
        });
        if settings.is_empty() || serde_json::to_vec(&ev).is_ok_and(|b| b.len() <= MAX_EVENT_BYTES)
        {
            return ev;
        }
        settings.pop();
    }
}
