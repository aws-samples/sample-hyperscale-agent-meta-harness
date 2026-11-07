//! Asynchronous jobs: the `research-job` runner and the lazy time limits `research-api` applies when a job is read.

use crate::archive::Filtered;
use crate::audit::{self, AuditDraft};
use crate::ops::put_item;
use crate::policy;
use crate::ports::{ArchiveSource, Blobs, Clock, Delta, Fail, IdKind, JobEvent, JobRecord, Table};
use crate::scan;
use harness_protocol::research::{
    BodyRef, ITEM_SCHEMA, ItemCaps, ItemKind, ItemManifest, ItemRef, JobState, ResearchError,
    ResearchErrorCode, ResearchLimits, ResearchOp, ResearchProvider, s3,
};
use sha2::{Digest, Sha256};
use std::fmt;

/// The archive filter: `archive::filter_archive` in the Lambda, a stand-in in a test.
pub type FilterFn = fn(&[u8], &[String], &ResearchLimits) -> Result<Filtered, Fail>;

/// What `research-job` runs on.
pub struct JobDeps<T, B, A, C> {
    pub table: T,
    pub blobs: B,
    pub source: A,
    pub clock: C,
    pub filter: FilterFn,
}

impl<T, B, A, C> fmt::Debug for JobDeps<T, B, A, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobDeps").finish_non_exhaustive()
    }
}

/// How one run of a job ended.
#[derive(Clone, Debug, PartialEq)]
pub enum JobEnd {
    Succeeded(ItemRef),
    Failed(ResearchError),
    /// Nothing was done: the job is gone or another invocation has it.
    Skipped(&'static str),
}

/// A 40-character lower-case hex string: a commit id.
fn is_commit(s: &str) -> bool {
    harness_protocol::common::is_lower_hex(s, 40)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Runs one queued job: reads its policy, downloads the archive, filters it, stores the body and then the manifest, and finishes the job.
/// It never returns an error: every failure finishes the job as `failed` with its word, so the caller of the route can read it.
pub async fn run_job<T, B, A, C>(deps: &JobDeps<T, B, A, C>, event: &JobEvent) -> JobEnd
where
    T: Table,
    B: Blobs,
    A: ArchiveSource,
    C: Clock,
{
    let now_ms = deps.clock.now_ms();
    let now_s = now_ms / 1000;
    let job = match deps.table.job_get(&event.run, &event.job_id).await {
        Ok(Some(j)) if j.agent == event.agent => j,
        Ok(_) => return JobEnd::Skipped("no such job"),
        Err(_) => return JobEnd::Skipped("the job could not be read"),
    };
    // A job that waited longer than the route's two minutes reads as `not_started` for everyone: it does not start late.
    if now_s.saturating_sub(job.created_s) > crate::ops::NOT_STARTED_S {
        let err = ResearchError::new(
            ResearchErrorCode::NotStarted,
            "the download did not start within 2 minutes",
        );
        return match deps
            .table
            .job_move(
                &job.run,
                &job.job_id,
                &[JobState::Queued],
                JobState::Failed,
                now_s,
                None,
                Some(&err),
            )
            .await
        {
            Ok(true) => JobEnd::Failed(err),
            _ => JobEnd::Skipped("the job is not queued"),
        };
    }
    match deps
        .table
        .job_move(
            &job.run,
            &job.job_id,
            &[JobState::Queued],
            JobState::Running,
            now_s,
            None,
            None,
        )
        .await
    {
        Ok(true) => {}
        _ => return JobEnd::Skipped("the job is not queued"),
    }
    match work(deps, &job, now_ms).await {
        Ok(item) => {
            let moved = deps
                .table
                .job_move(
                    &job.run,
                    &job.job_id,
                    &[JobState::Running],
                    JobState::Succeeded,
                    deps.clock.now_ms() / 1000,
                    Some(&item),
                    None,
                )
                .await;
            match moved {
                Ok(true) => {
                    // The completion audit object says what the job's own state says.
                    audit_end(deps, &job, now_ms, Some(&item), None).await;
                    JobEnd::Succeeded(item)
                }
                _ => JobEnd::Skipped("the job was finished by another writer"),
            }
        }
        Err(fail) => {
            let err: ResearchError = fail.clone().into();
            let _ = deps
                .table
                .job_move(
                    &job.run,
                    &job.job_id,
                    &[JobState::Running],
                    JobState::Failed,
                    deps.clock.now_ms() / 1000,
                    None,
                    Some(&err),
                )
                .await;
            audit_end(deps, &job, now_ms, None, Some(fail.code)).await;
            JobEnd::Failed(err)
        }
    }
}

async fn work<T, B, A, C>(
    deps: &JobDeps<T, B, A, C>,
    job: &JobRecord,
    now_ms: u64,
) -> Result<ItemRef, Fail>
where
    T: Table,
    B: Blobs,
    A: ArchiveSource,
    C: Clock,
{
    let policy = policy::active(&deps.table, &job.run, now_ms / 1000).await?;
    let limits = &policy.limits;
    let repo = scan::github_repo(&job.url)?;
    let downloaded = deps
        .source
        .download(&repo, job.git_ref.as_deref(), limits.repo_download_bytes)
        .await?;
    if downloaded.final_host != "codeload.github.com" {
        return Err(Fail::upstream("the archive came from an unexpected host"));
    }
    let filtered = (deps.filter)(&downloaded.bytes, &job.paths, limits)?;
    if let Some(r) = &job.git_ref
        && is_commit(r)
        && *r != filtered.commit
    {
        return Err(Fail::upstream("the archive's commit is not the pinned ref"));
    }
    // The policy is read again before anything is stored: research may have been switched off while the download ran, and the limits
    // the run holds now are the ones the result is held to.
    let policy = policy::active(&deps.table, &job.run, deps.clock.now_ms() / 1000).await?;
    let limits = &policy.limits;
    if filtered.tar_gz.len() as u64 > limits.repo_compressed_bytes
        || filtered.files.len() as u64 > u64::from(limits.repo_files)
        || filtered.unpacked_bytes > limits.repo_unpacked_bytes
    {
        return Err(Fail::new(
            ResearchErrorCode::TooLarge,
            "the filtered archive is over its cap",
        ));
    }
    deps.table
        .reserve(
            &job.run,
            &Delta {
                context_bytes: filtered.tar_gz.len() as u64,
                ..Delta::default()
            },
            limits,
        )
        .await?;
    let item_id = s3::item_id(deps.table.next_id(&job.run, IdKind::Item).await?);
    let body_key = s3::body_key(&job.run, &item_id, ItemKind::Repo);
    let body_sha = sha256_hex(&filtered.tar_gz);
    let bytes = filtered.tar_gz.len() as u64;
    let manifest = ItemManifest {
        schema: ITEM_SCHEMA.to_string(),
        item_id: item_id.clone(),
        kind: ItemKind::Repo,
        run: job.run.clone(),
        agent: job.agent.clone(),
        provider: ResearchProvider::GithubArchive,
        request_id: format!("job-{}", job.job_id),
        origin: repo.origin(),
        commit: Some(filtered.commit.clone()),
        git_ref: job.git_ref.clone(),
        retrieved_at: audit::rfc3339(now_ms),
        licence: filtered.licence.clone(),
        body: BodyRef {
            key: body_key.clone(),
            sha256: body_sha.clone(),
            bytes,
        },
        source_sha256: None,
        files: filtered.files.clone(),
        dropped: filtered.dropped.clone(),
        caps: Some(ItemCaps {
            max_files: limits.repo_files,
            max_unpacked_bytes: limits.repo_unpacked_bytes,
        }),
        truncated: false,
    };
    manifest.validate().map_err(|e| Fail::internal(e.message))?;
    put_item(&deps.blobs, &body_key, filtered.tar_gz, "application/gzip").await?;
    let manifest_key = s3::manifest_key(&job.run, &item_id);
    put_item(
        &deps.blobs,
        &manifest_key,
        manifest.canonical_bytes(),
        "application/json",
    )
    .await?;
    Ok(ItemRef {
        id: item_id,
        kind: ItemKind::Repo,
        origin: repo.origin(),
        manifest_key,
        manifest_sha256: manifest.sha256(),
        body_key,
        body_sha256: body_sha,
        bytes,
        licence: Some(filtered.licence.spdx),
        truncated: false,
        files: Some(u32::try_from(filtered.files.len()).unwrap_or(u32::MAX)),
        dropped: Some(u32::try_from(filtered.dropped.len()).unwrap_or(u32::MAX)),
    })
}

/// The completion audit object of a job: the start call recorded the request, this records how the download ended.
async fn audit_end<T, B, A, C>(
    deps: &JobDeps<T, B, A, C>,
    job: &JobRecord,
    started_ms: u64,
    item: Option<&ItemRef>,
    code: Option<ResearchErrorCode>,
) where
    T: Table,
    B: Blobs,
    A: ArchiveSource,
    C: Clock,
{
    let mut draft = AuditDraft::new(ResearchOp::Repo);
    draft.input.urls = vec![job.url.clone()];
    draft.input.git_ref = job.git_ref.clone();
    draft.input.paths = job.paths.clone();
    draft.job_id = Some(job.job_id.clone());
    if let Some(i) = item {
        draft.items.push(i.id.clone());
    }
    let now = deps.clock.now_ms();
    let obj = audit::build(
        &job.run,
        &job.agent,
        ResearchProvider::GithubArchive,
        &format!("job-{}", job.job_id),
        now,
        now.saturating_sub(started_ms),
        code,
        draft,
    );
    // The record of the end is best effort: the job's own state is the authority.
    let _ = audit::write(&deps.blobs, &obj).await;
}
