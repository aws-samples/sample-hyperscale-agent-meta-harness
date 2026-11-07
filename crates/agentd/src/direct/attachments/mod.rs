//! Module `attachments`: a run may carry files of the user's own that the engine reads as context. `ham ... --attach
//! <file>` sends them in the `attachments` member of the start request (`harness_protocol::direct::Attachment`, one validator for the three
//! ends); this module puts them where the engine reads them and tells the engine where, before any model call.
//!
//! What `on_startup` does (an `Err` ends the run `stopped_setup_failed`, "did not start", before any model call):
//!
//! 1. A request with no attachments stops here: nothing is created, no variable is set, no event and no log line are written, so a run
//!    without attachments is the run it was (the 13 golden files do not move).
//! 2. The request's attachments pass the shared validator (`check_attachments`); a refusal names the item and the rule, never the text.
//! 3. [`place::place`] writes them read-only into `<agent data>/context/attachments/` (outside the work tree, so nothing in them reaches a
//!    snapshot, the zip or a pull request) with an index in the columns of the lifecycle pack's context index.
//! 4. The engine gets `HAMH_ATTACHMENTS_DIR`, and one `turn.progress` event of kind `attachments` names the files (count, bytes, names; never
//!    the text). The user's prompt is not changed (root invariant 1): the engine learns of the files from the directory, the variable and
//!    the rules.
//!
//! The module does not depend on the research module: a stage or a run with research off still gets the user's own files. The exact-copy
//! delivery rule never reads them (`delivery::third_party`): they are the user's, not material the research route stored.

pub mod place;

use crate::BoxFuture;
use crate::direct::compat;
use crate::direct::context::ContextDir;
use crate::direct::hooks::{DirectHook, HookEnv, StartupCtx};
use crate::time::{format_rfc3339, now_unix};
use harness_protocol::direct::{DIRECT_TURN, check_attachments};
use harness_protocol::envelope::EventType;
use serde_json::json;
use std::sync::Arc;

/// The environment variable that names the directory for the engine.
pub const ENV_DIR: &str = "HAMH_ATTACHMENTS_DIR";

/// The module, always registered: it acts only when the request carries attachments. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    Some(Arc::new(AttachmentsHook::new()))
}

/// The module.
#[derive(Debug, Default)]
pub struct AttachmentsHook;

impl AttachmentsHook {
    pub fn new() -> Self {
        Self
    }
}

impl DirectHook for AttachmentsHook {
    fn name(&self) -> &'static str {
        "attachments"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let items = &ctx.request.attachments;
            if items.is_empty() {
                return Ok(());
            }
            check_attachments(items).map_err(|e| e.to_string())?;
            let context = ContextDir::create(&ctx.env.agent_data)?;
            let placed = place::place(
                &context,
                &ctx.env.run_id,
                items,
                &format_rfc3339(now_unix()),
            )?;
            // COMPAT-CAH-DATA: both prefixes (rules the user edited before the rename name the earlier one).
            compat::set_both(
                ctx.engine_env,
                "ATTACHMENTS_DIR",
                placed.dir.display().to_string(),
            );
            let bytes: u64 = placed.items.iter().map(|i| i.bytes).sum();
            let names: Vec<&str> = placed.items.iter().map(|i| i.name.as_str()).collect();
            ctx.env.log.line(&format!(
                "direct attachments placed: {} file(s), {bytes} bytes",
                placed.items.len()
            ));
            ctx.env.rec.record(
                EventType::TurnProgress,
                json!({"turn": DIRECT_TURN, "kind": "attachments", "count": placed.items.len(), "bytes": bytes, "names": names}),
            );
            Ok(())
        })
    }
}
