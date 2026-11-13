//! Module `continue_from`. A follow-up run starts from the exact head an earlier run delivered. The repo-gateway
//! stages the source run's verified bundle as THIS run's `input/repo.bundle` (the run's credentials read only its own prefix, and no IAM
//! change was wanted), with the delivered head checked out, so the repository agentd restores IS the seed and the driver needs no new path.
//! The start request carries `continueFrom { runId, headSha }`; control-api stores the head it resolved.
//!
//! What agentd adds is one tripwire in `on_startup`, before the engine starts and before any prompt (an `Err` ends the run `stopped_setup_failed`,
//! "did not start"): the restored HEAD equals the stored `headSha` and the history is whole (a shallow history breaks the merge base, the diff and
//! the review of the follow-up). A restored repository that is at another commit than the one the user was shown must never be worked on.
//! A second invocation of the same run on the same VM finds the work tree where the first attempt left it: after the first passage a file in the
//! configuration directory says so, and then HEAD may also be a commit with the seed in its history (`resumed_check`).
//! One `turn.progress` event, kind `continue-from`, records the source run and the seed commit. The review target of the run is its own base
//! (`summary.json` git.baseSha is the seed, because the work tree is created from the seed), so the seed stays a separate fact from what the
//! run changes.
//!
//! Not checked here, on purpose: a clean work tree. The driver sets repository hook paths aside before the module runs, which makes a clean
//! tree look dirty. And the prompt: the engine receives the user's text as typed (invariant 1); nothing about the earlier run is added to it.
//!
//! The module has no setting: it acts only on a request that carries `continueFrom`.

use crate::BoxFuture;
use crate::direct::classify::scrub_detail;
use crate::direct::hooks::{DirectHook, HookEnv, StartupCtx};
use harness_protocol::direct::{DIRECT_TURN, check_continue_from};
use harness_protocol::envelope::EventType;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// The history of `repo` is whole: a shallow history breaks the merge base, the diff and the review of the follow-up.
async fn whole_history(repo: &Path) -> Result<(), String> {
    let shallow =
        crate::workspace::git_with_env(repo, &["rev-parse", "--is-shallow-repository"], &[])
            .await
            .map_err(|e| format!("continue_from: {e}"))?;
    if shallow != "false" {
        return Err(
            "continue_from: the seed repository is shallow; a follow-up needs the whole history"
                .to_string(),
        );
    }
    Ok(())
}

async fn head_of(repo: &Path) -> Result<String, String> {
    crate::workspace::git_with_env(repo, &["rev-parse", "HEAD"], &[])
        .await
        .map_err(|e| format!("continue_from: {e}"))
}

fn not_the_seed(head: &str, head_sha: &str) -> String {
    scrub_detail(&format!(
        "continue_from: the repository the run restored is at {}, not the seed commit {}",
        short(head),
        short(head_sha)
    ))
}

/// The restored repository is at `head_sha` (the full commit id, lowercase) and its history is whole. This is the check of a fresh restore:
/// the bundle the repo-gateway staged has the seed checked out, so any other commit is a restore that went wrong.
pub async fn seed_check(repo: &Path, head_sha: &str) -> Result<(), String> {
    let head = head_of(repo).await?;
    if head != head_sha {
        return Err(not_the_seed(&head, head_sha));
    }
    whole_history(repo).await
}

/// The check of a second invocation of the same run on the same VM: the work tree is where the first attempt left it, so `HEAD` is the seed
/// or a commit with the seed in its history, and the history is whole.
pub async fn resumed_check(repo: &Path, head_sha: &str) -> Result<(), String> {
    let head = head_of(repo).await?;
    if head != head_sha {
        let descends = crate::workspace::git_with_env(
            repo,
            &["merge-base", "--is-ancestor", head_sha, "HEAD"],
            &[],
        )
        .await;
        if descends.is_err() {
            return Err(not_the_seed(&head, head_sha));
        }
    }
    whole_history(repo).await
}

/// The file in the configuration directory (root-owned in the microVM, so the engine cannot forge it) that says this run has passed the
/// seed check once, for this source run and this seed.
const SEEDED_FILE: &str = "agentd-continue-from.json";

fn seeded_text(from_run: &str, head_sha: &str) -> String {
    format!("{{\"fromRun\":\"{from_run}\",\"seed\":\"{head_sha}\"}}\n")
}

/// The module.
#[derive(Debug, Default)]
pub struct ContinueFromHook;

impl ContinueFromHook {
    pub fn new() -> Self {
        Self
    }

    async fn startup(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let Some(from) = &ctx.request.continue_from else {
            return Ok(());
        };
        check_continue_from(from).map_err(|e| format!("continue_from: {e}"))?;
        let Some(head) = from.head_sha.as_deref() else {
            return Err(
                "continue_from: the stored request has no headSha; control-api stores the delivered head it resolved"
                    .to_string(),
            );
        };
        // A second invocation of the same run on the same VM finds the work tree where the first attempt left it. The first passage leaves a
        // file in the configuration directory; with it the work tree may be ahead of the seed, without it (a fresh restore) it must be the seed.
        let seeded = ctx.config_dir.join(SEEDED_FILE);
        let wanted = seeded_text(&from.run_id, head);
        // A slice after the first restores an earlier slice's work, which is ahead of the seed by what that slice did.
        if ctx.env.turn > 1 || std::fs::read_to_string(&seeded).is_ok_and(|t| t == wanted) {
            resumed_check(ctx.repo, head).await?;
        } else {
            seed_check(ctx.repo, head).await?;
            let _ = std::fs::remove_file(&seeded);
            if std::fs::write(&seeded, &wanted).is_ok() {
                let _ = std::fs::set_permissions(&seeded, std::fs::Permissions::from_mode(0o444));
                let _ = crate::engine::direct::own_by_root(&seeded);
            }
        }
        ctx.env.rec.record(
            EventType::TurnProgress,
            json!({
                "turn": DIRECT_TURN,
                "kind": "continue-from",
                "fromRun": from.run_id,
                "seed": head,
            }),
        );
        Ok(())
    }
}

impl DirectHook for ContinueFromHook {
    fn name(&self) -> &'static str {
        "continue_from"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.startup(ctx))
    }
}

/// The module. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    Some(Arc::new(ContinueFromHook::new()))
}
