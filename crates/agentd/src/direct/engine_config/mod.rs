//! Module `engine_config`: the user's settings for the engine, from the `engineConfig`
//! member of the start request. The driver applies them where it writes the engine's configuration (`direct/mod.rs` startup steps 7 and 9:
//! the compaction block of the provider file, the `--thinking` argument, the command time limit variable and the union of the tools switched off,
//! all through `crate::engine::user_settings`, which holds the engine's words). This module is the seam's part: it proves the placement, tells the
//! card, and fills the summary.
//!
//! `on_startup`, after the driver wrote everything and the configuration directory became root's:
//!
//! 0. An engine whose launch always carries a command time limit has its placement read back first, with a member or
//!    without (`check_command_limit`: the plugin file and both layers of the session document). A mismatch for a limit the user set ends the run
//!    the way every read-back mismatch does. A mismatch for the harness's own default does not: the run is told once (`turn.progress`, kind
//!    `notify`, reason `command_limit`), one log line is written, the wait bound is set for the engine's own limit, and the run goes on. For the
//!    engine that carries a limit only when the user set it, this step looks at nothing.
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
//!    confirmed it. For the second engine's command time limit the engine's own sign is looked for after `session/new` (`Respawn::confirm`).
//!
//! `on_summary` fills `DirectSummary.engineConfig` (engine, schema, the settings hash and the key count, never a value) when step 4 happened.
//! The module votes on nothing and the user's prompt is not changed (root invariant 1).

use crate::BoxFuture;
use crate::direct::hooks::{DirectHook, HookEnv, StartupCtx, SummaryCtx};
use crate::direct::request::engine_settings;
use crate::engine::direct::{LIMIT_REASON, WAIT_ENV};
use crate::engine::user_settings::{Placed, check_command_limit, read_back};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DIRECT_TURN, DirectEngineConfigRef, EngineConfig, keys_for};
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
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

/// What the module's startup reads and changes: the parts of a [`StartupCtx`] it uses.
pub struct StartParts<'a> {
    pub env: &'a HookEnv,
    pub request: &'a StartDirectRunRequest,
    pub config_dir: &'a Path,
    pub data_dir: &'a Path,
    /// The engine's argv.
    pub argv: &'a [String],
    /// The environment the engine will get; step 0 may lower the wait bound in it.
    pub engine_env: &'a mut BTreeMap<String, String>,
}

impl std::fmt::Debug for StartParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The engine's argv and environment are left out: they are the engine's launch, and a log line has no use for them.
        f.debug_struct("StartParts")
            .field("engine", &self.env.engine)
            .field("config_dir", &self.config_dir)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
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

    /// The module's startup on the parts of a [`StartupCtx`] it reads and changes (steps 0 to 4 of the module's description). `on_startup`
    /// calls it with the context's own parts; a test calls it for any engine without a whole run.
    pub fn start_with(&self, parts: StartParts<'_>) -> Result<(), String> {
        let StartParts {
            env,
            request,
            config_dir,
            data_dir,
            argv,
            engine_env,
        } = parts;
        let settings = engine_settings(request, &env.engine)?;
        match limit_step(&env.engine, config_dir, &*engine_env, &settings) {
            LimitStep::Held => {}
            LimitStep::Ended(why) => return Err(why),
            LimitStep::Told { notice, wait_s } => {
                env.log
                    .line("direct engine command limit not placed: the engine's own limit applies");
                env.rec
                    .record(EventType::TurnProgress, limit_notice(&notice));
                engine_env.insert(WAIT_ENV.into(), wait_s.to_string());
            }
        }
        let Some(cfg) = &request.engine_config else {
            return Ok(());
        };
        if cfg.key_count() == 0 {
            return Ok(());
        }
        read_back(
            &Placed {
                engine: &env.engine,
                config_dir,
                data_dir,
                argv,
                env: &*engine_env,
                stage_disabled: &env.cfg.direct.disabled_tools,
            },
            &settings,
        )?;
        let reference = cfg.to_ref();
        env.log.line(&format!(
            "direct engine settings placed: {} key(s), read back ok",
            reference.keys
        ));
        env.rec
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
        Box::pin(async move {
            let argv = ctx.engine_argv().to_vec();
            self.start_with(StartParts {
                env: ctx.env,
                request: ctx.request,
                config_dir: ctx.config_dir,
                data_dir: ctx.data_dir,
                argv: &argv,
                engine_env: &mut *ctx.engine_env,
            })
        })
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            ctx.summary.engine_config = self.placed();
        })
    }
}

/// What step 0 of the startup found about the command time limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitStep {
    /// The placement was read back, or this engine has none to read.
    Held,
    /// The user's own limit could not be read back: as every user setting, the run does not start. The text names the part and no value.
    Ended(String),
    /// The harness's own default could not be placed: the run is told `notice` once and goes on with the engine's own limit; `wait_s` is the
    /// bound of a wait that fits that limit.
    Told { notice: String, wait_s: u32 },
}

/// Step 0 of the startup: reads the placement of the command time limit back and says what follows. Only a limit the user set ends a run.
pub fn limit_step(
    engine: &str,
    config_dir: &Path,
    engine_env: &BTreeMap<String, String>,
    settings: &crate::engine::user_settings::UserSettings,
) -> LimitStep {
    match check_command_limit(engine, config_dir, engine_env, settings) {
        Ok(()) => LimitStep::Held,
        Err(why) if settings.command_timeout.is_some() => LimitStep::Ended(why),
        Err(_) => {
            let (notice, wait_s) = crate::engine::config::limit_not_placed(engine, settings);
            LimitStep::Told { notice, wait_s }
        }
    }
}

/// The notice of [`LimitStep::Told`], in the shape every notice of a run has.
pub fn limit_notice(detail: &str) -> Value {
    json!({
        "turn": DIRECT_TURN,
        "kind": "notify",
        "reason": LIMIT_REASON,
        "outageMinutes": null,
        "detail": detail,
    })
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
