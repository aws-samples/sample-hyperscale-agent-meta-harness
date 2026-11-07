//! `prepare`: reads the run and its stored request, refuses what can never run, and builds
//! the repository-gateway payloads exactly as the old StartRun did (`run_tasks.rs` StartRun), so the unchanged gateway
//! stages the base the session restores.

use crate::error::RunSessionError;
use crate::ops::{Deps, ResearchStage};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use dispatcher::items::sha256_hex;
use ham_core::models::builtin;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DirectModelSpec, s3};
use harness_protocol::goal::GoalStatus;
use harness_protocol::research::PolicyItem;
use harness_protocol::target::{RepoTarget, RunTarget};
use serde::Serialize;
use serde_json::{Value, json};
use worker_api::handler::Minter;

/// What `prepare` answers; `newRepo` is present for a `--new-repo` run only.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_repo: Option<Value>,
    pub stage: Value,
    pub target: RunTarget,
    /// The repository name of the zip: the CodeCommit name, or `s3-only`.
    pub repo: String,
    pub model: DirectModelSpec,
}

/// The https URLs the prompt names, for the research route's provenance gate: a page the user pointed at needs no search first. Found by
/// splitting on whitespace and the brackets and quotes that wrap links, trimming trailing punctuation; only `https://`, each at most
/// `max_bytes`, in order, without repeats, at most [`harness_protocol::research::MAX_PROMPT_URLS`]. The route canonicalises them when it
/// compares, and applies its own URL rules to a fetch.
pub fn prompt_urls(prompt: &str, max_bytes: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let delimiters = |c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'' | '`')
    };
    for token in prompt.split(delimiters) {
        let token = token.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if token.starts_with("https://")
            && token.len() <= max_bytes
            && !out.iter().any(|u| u == token)
        {
            out.push(token.to_string());
            if out.len() >= harness_protocol::research::MAX_PROMPT_URLS {
                break;
            }
        }
    }
    out
}

/// The run's research policy row from its request and the stage settings: on unless the request says `enabled: false`, the stage's provider
/// and limits, an expiry an hour past the longest session, and the URLs of the prompt.
pub fn research_policy(
    run_id: &str,
    request: &StartDirectRunRequest,
    stage: &ResearchStage,
    now_s: u64,
    max_lifetime_s: u64,
) -> PolicyItem {
    let enabled = request.research.is_none_or(|r| r.enabled);
    PolicyItem::new(
        run_id,
        enabled,
        stage.provider,
        stage.limits.clone(),
        now_s + max_lifetime_s + 3600,
    )
    .with_prompt_urls(prompt_urls(
        &request.prompt,
        stage.limits.url_bytes as usize,
    ))
}

/// The `<repo>` of a zip name (`run_tasks::repo_name`).
pub fn repo_name(t: &RunTarget) -> String {
    match t {
        RunTarget::Repo { repo } | RunTarget::NewRepo { repo } => match repo {
            RepoTarget::CodeCommit { name } | RepoTarget::GitHub { name, .. } => name.clone(),
            RepoTarget::GitLab { path } => path.rsplit('/').next().unwrap_or(path).to_string(),
        },
        RunTarget::S3Only => "s3-only".into(),
    }
}

/// The run's stored request, checked against its `run.created` event.
pub async fn read_request<S, B>(
    store: &S,
    blobs: &B,
    run_id: &str,
) -> Res<(StartDirectRunRequest, String)>
where
    S: Store,
    B: Blobs,
{
    let key = s3::request_key(run_id);
    let body = blobs
        .get(&key)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no {key}")))?;
    let sha = sha256_bytes(&body);
    match store.run_created_request_sha256(run_id).await? {
        Some(want) if want == sha => {}
        Some(_) => {
            return Err(RunSessionError::Invalid(format!(
                "{key} differs from the request run.created recorded"
            )));
        }
        None => {
            return Err(RunSessionError::Invalid(format!(
                "run {run_id} has no run.created event to check {key} against"
            )));
        }
    }
    let request: StartDirectRunRequest = serde_json::from_slice(&body)
        .map_err(|e| RunSessionError::Invalid(format!("{key}: {e}")))?;
    Ok((request, sha))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The model row of `models.json` for the request's model, as the session needs it.
pub fn model_spec(model: &str) -> Res<DirectModelSpec> {
    let m = builtin()
        .check(model)
        .map_err(|e| RunSessionError::Invalid(e.to_string()))?;
    Ok(DirectModelSpec {
        id: m.id.clone(),
        region: m.region.clone(),
        protocol: m.protocol,
        context_tokens: m.context_tokens,
        max_output_tokens: m.max_output_tokens,
        default_reasoning_effort: m.reasoning_default().map(str::to_string),
    })
}

/// The routes agentd may switch to when the run's own route fails, for the request's model:
/// the model's other profile first, then the other supported models (`Models::failover_routes`). Every route has a smoke record.
pub fn failover_specs(model: &str) -> Vec<DirectModelSpec> {
    builtin().failover_routes(model)
}

pub async fn prepare<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str) -> Res<Prepared>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let run = deps
        .store
        .get_run(run_id)
        .await?
        .ok_or_else(|| RunSessionError::Invalid(format!("run {run_id} has no Run item")))?;
    if run.status != GoalStatus::Active {
        return Err(RunSessionError::Invalid(format!(
            "run {run_id} is {}; only an active run starts",
            serde_json::to_value(run.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        )));
    }
    let (request, _sha) = read_request(&deps.store, &deps.blobs, run_id).await?;
    if sha256_hex(&request.prompt) != run.prompt_sha256 {
        return Err(RunSessionError::Invalid(format!(
            "the stored request's prompt differs from the Run item of {run_id}"
        )));
    }
    if serde_json::to_value(&request.target).ok() != serde_json::to_value(&run.target).ok() {
        return Err(RunSessionError::Invalid(format!(
            "the stored request's target differs from the Run item of {run_id}"
        )));
    }
    let model = model_spec(&request.model)?;
    let base_ref = request.base_ref.clone().or(run.base_ref.clone());
    // A follow-up starts from the head an earlier run delivered. control-api checked the source and stored the head it
    // resolved; the stored request is a file the control plane wrote, so the owner, the target and the head are checked again here. The
    // target is `s3-only` with no `baseRef`, or the source run's own repository with no `baseRef` or its branch `harness/<source run>`. The
    // repo-gateway loads the source's verified bundle and refuses a head it does not hold.
    let from_run = match &request.continue_from {
        None => None,
        Some(c) => {
            let bad =
                |why: &str| RunSessionError::Invalid(format!("continueFrom {}: {why}", c.run_id));
            match &run.target {
                RunTarget::S3Only if base_ref.is_some() => {
                    return Err(bad("a follow-up on s3-only has no baseRef"));
                }
                RunTarget::NewRepo { .. } => {
                    return Err(bad(
                        "a follow-up targets the repository itself, not --new-repo",
                    ));
                }
                RunTarget::Repo { .. } => {
                    let own = [
                        format!("harness/{}", c.run_id),
                        format!("refs/heads/harness/{}", c.run_id),
                    ];
                    if let Some(b) = &base_ref
                        && !own.contains(b)
                    {
                        return Err(bad(&format!(
                            "a repository follow-up names no baseRef but harness/{}, not {b}",
                            c.run_id
                        )));
                    }
                }
                RunTarget::S3Only => {}
            }
            let Some(head) = c.head_sha.clone() else {
                return Err(bad("the stored request has no resolved head"));
            };
            let source = match deps.store.get_run(&c.run_id).await? {
                Some(source) if source.owner_principal_arn == run.owner_principal_arn => source,
                _ => return Err(bad("the source run is not a run of this run's owner")),
            };
            if let RunTarget::Repo { repo } = &run.target {
                let same = match &source.target {
                    RunTarget::Repo { repo: r } | RunTarget::NewRepo { repo: r } => r == repo,
                    RunTarget::S3Only => false,
                };
                if !same {
                    return Err(bad(&format!(
                        "a follow-up on {repo} continues a run of that repository; the source run targeted {}",
                        ham_core::run_index::target_text(&source.target)
                    )));
                }
            }
            Some(json!({"runId": c.run_id, "headSha": head}))
        }
    };
    let stage_of = |target: Value| {
        let mut v = json!({"operation": "stage", "runId": run_id, "target": target});
        if let Some(b) = &base_ref {
            v["baseRef"] = json!(b);
        }
        if let Some(f) = &from_run {
            v["fromRun"] = f.clone();
        }
        v
    };
    let to_json = |repo: &RepoTarget| {
        serde_json::to_value(repo).map_err(|e| RunSessionError::Invalid(e.to_string()))
    };
    // The research policy row. Research is an optional extra: a row that cannot be written leaves research off for this run (the
    // route answers `disabled` to a run without a row) and never stops the run.
    if let Some(stage) = &deps.cfg.research {
        let row = research_policy(
            run_id,
            &request,
            stage,
            deps.cfg.now_ms() / 1000,
            deps.cfg.max_lifetime_s,
        );
        if let Err(e) = deps.store.put_research_policy(&row).await {
            println!(
                "{}",
                json!({"function": "run-session", "op": "prepare", "runId": run_id, "research": "policy row not written", "reason": e.to_string()})
            );
        }
    }
    let (new_repo, stage) = match &run.target {
        RunTarget::Repo { repo } => (None, stage_of(to_json(repo)?)),
        RunTarget::NewRepo { repo } => (
            Some(json!({"operation": "new-repo", "runId": run_id, "target": to_json(repo)?})),
            stage_of(to_json(repo)?),
        ),
        // repo-gateway writes the empty base for the run target itself (no repository call).
        RunTarget::S3Only => (None, stage_of(json!({"mode": "s3-only"}))),
    };
    Ok(Prepared {
        new_repo,
        stage,
        repo: repo_name(&run.target),
        target: run.target.clone(),
        model,
    })
}
