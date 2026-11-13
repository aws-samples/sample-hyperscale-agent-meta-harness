//! `repo-gateway`, the only holder of git write access.
//! The Step Functions task input names one operation:
//!
//! - `stage`: check the target, fetch `baseRef` and write a full bundle (with `HEAD`)
//!   to `runs/<run>/input/repo.bundle`. For the `--s3-only` run target it makes no CodeCommit call and no
//!   allowlist read: the base is one empty-tree root commit on `main` at [`EMPTY_BASE_UNIX`].
//! - `new-repo`: `CreateRepository hamh-e2e-new-<yyyymmdd>` with the four project tags
//!   in the request, then an empty base commit on `main`. The role may not push `main`,
//!   so the commit is pushed to `refs/heads/harness/<runId>` and `main` is created on
//!   it with `CreateBranch`, which cannot move an existing branch.
//! - `push-pr` (P15, C5): push the verified bundle's head to `refs/heads/harness/<runId>` only,
//!   open one pull request to `main` and attach `hamh-prcheck-required` when the head has a root `buildspec.yml` for
//!   the check to run, then post the table comments, except on a
//!   pull request with no commits, which CodeCommit opens but takes no comment on.
//!
//! Target checks come first, in this order and before any repository read or push: scheme and
//! the stage's repository-name rule (default `hamh-e2e-*`), the allowlist item, then `GetRepository` and the ARN's account and Region.

use crate::bundle::load_verified;
use crate::checks::{
    BUILD_FILE, DESTINATION_BRANCH, DESTINATION_REF, HarnessRef, NO_CHECK_NOTE, PRCHECK_RULE,
    check_allowlisted, check_comment, check_head_sha, check_new_repo_name, check_pr_text,
    check_repository_arn, codecommit_name_in, harness_ref, is_build_file, normalize_base_ref,
    pr_console_url, prcheck_rule_content, pull_request_id_in, push_ref,
};
use crate::credential::rfc3339;
use crate::error::DeliveryError;
use crate::files::{scratch, sha256_file};
use crate::follow_up::{
    Aim, Facts, FollowUp, FollowUpOpened, Landed, Plan, decide, link_note, target_note,
};
use crate::git::{Git, Remote};
use crate::ports::{Allowlist, Blobs, PutMeta};
use crate::repo_names::RepoNames;
use harness_protocol::store::{RepoAllowItem, s3};
use harness_protocol::target::{E2E_ACCOUNT, E2E_REGION, RepoTarget, RunTarget};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};

pub const PROJECT: &str = "hyperscale-agent-meta-harness";
/// `managed-by` on repositories the gateway creates (teardown deletes `hamh-e2e-*`).
pub const MANAGED_BY: &str = "repo-gateway";
/// 2020-01-01T00:00:00Z: author and committer time of the `--s3-only` empty base, so a rerun
/// writes the same bundle byte for byte.
pub const EMPTY_BASE_UNIX: u64 = 1_577_836_800;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMeta {
    pub name: String,
    pub arn: Option<String>,
    pub account_id: Option<String>,
    pub default_branch: Option<String>,
    /// `new-repo` writes [`new_repo_description`]; it names the run that created the repository.
    pub description: Option<String>,
}

/// The description `new-repo` gives the repository it creates: how a retry recognizes its own.
pub fn new_repo_description(run_id: &str) -> String {
    format!("ham --new-repo for run {run_id}")
}

/// The message of the empty base commit `new-repo` and the `--s3-only` stage write.
fn empty_base_message(run_id: &str) -> String {
    format!("ham: empty base for run {run_id}")
}

/// Whether a create call made the resource or found it (a retry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Created {
    Created,
    Existed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPullRequest {
    pub repository: String,
    pub title: String,
    pub description: String,
    pub source_ref: String,
    pub destination_ref: String,
    /// The same token returns the first request's pull request (CodeCommit idempotency).
    pub client_request_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestInfo {
    pub pull_request_id: String,
    pub revision_id: String,
    pub source_commit: String,
    pub destination_commit: String,
    /// The repository the pull request belongs to (`GetPullRequest` takes only an id, so the caller checks it).
    pub repository: String,
    /// The full reference names of the first target, `refs/heads/<branch>`.
    pub source_ref: String,
    pub destination_ref: String,
    /// The most recent commit the two branches have in common.
    pub merge_base: String,
    /// `mergeMetadata.isMerged`: the pull request was merged, with any strategy.
    pub merged: bool,
    /// `(name, content)` of every approval rule.
    pub approval_rules: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewComment {
    pub repository: String,
    pub pull_request_id: String,
    pub before_commit_id: String,
    pub after_commit_id: String,
    pub content: String,
    pub client_request_token: String,
}

/// The CodeCommit API calls the gateway makes (`crate::aws::CodeCommitApi`).
pub trait CodeCommit: Send + Sync {
    /// `None` when the repository does not exist.
    fn get_repository(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Option<RepoMeta>, DeliveryError>> + Send;
    fn create_repository(
        &self,
        name: &str,
        description: &str,
        tags: &[(String, String)],
    ) -> impl Future<Output = Result<Created, DeliveryError>> + Send;
    fn create_branch(
        &self,
        repository: &str,
        branch: &str,
        commit_id: &str,
    ) -> impl Future<Output = Result<Created, DeliveryError>> + Send;
    fn update_default_branch(
        &self,
        repository: &str,
        branch: &str,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send;
    fn create_pull_request(
        &self,
        pr: &NewPullRequest,
    ) -> impl Future<Output = Result<PullRequestInfo, DeliveryError>> + Send;
    fn get_pull_request(
        &self,
        pull_request_id: &str,
    ) -> impl Future<Output = Result<PullRequestInfo, DeliveryError>> + Send;
    fn create_approval_rule(
        &self,
        pull_request_id: &str,
        name: &str,
        content: &str,
    ) -> impl Future<Output = Result<Created, DeliveryError>> + Send;
    fn post_comment(
        &self,
        c: &NewComment,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send;
}

/// The git remote of a repository, with a freshly signed credential in AWS.
pub trait Remotes: Send + Sync {
    fn remote(
        &self,
        repository: &str,
    ) -> impl Future<Output = Result<Remote, DeliveryError>> + Send;
}

/// What `stage` reads: a repository target string (`cc:<name>`) or the run target of the run item
/// (`{"mode": "repo" | "new-repo", "repo": ...}` stages that repository; `{"mode": "s3-only"}` the
/// empty base).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum StageTarget {
    Repo(RepoTarget),
    Run(RunTarget),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageRequest {
    pub run_id: String,
    pub target: StageTarget,
    /// A branch; absent means the repository's default branch.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Start from the head an earlier run delivered (`ham continue`, the chat's follow-up): with the `s3-only` target and
    /// no `base_ref`, or with a repository target and no `base_ref` or `harness/<source run>`.
    #[serde(default)]
    pub from_run: Option<FromRun>,
    /// The container signals of the run (lines of `ham_core::container_signals`; `default` stands for the built-in list). run-session sends the
    /// member only when the run could end up on the Instances machine. Present, `stage` reads the tree of the base commit and answers
    /// `containerCheck`; absent, it checks nothing and adds no member.
    #[serde(default)]
    pub container_signals: Option<Vec<String>>,
}

/// The source of a follow-up run: the verified bundle of `run_id` must hold `head_sha` on `refs/heads/harness/<run_id>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FromRun {
    pub run_id: String,
    pub head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRepoRequest {
    pub run_id: String,
    pub target: RepoTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPrRequest {
    pub run_id: String,
    pub target: RepoTarget,
    pub head_sha: String,
    pub title: String,
    pub description: String,
    /// The full PR-body tables, one comment each.
    #[serde(default)]
    pub comments: Vec<String>,
    /// Optional; anything but `refs/heads/harness/<runId>` is refused.
    #[serde(default)]
    pub source_ref: Option<String>,
    /// The earlier run this run continues. The pull request then points where `follow_up::decide` says, and the two pull
    /// requests link to each other.
    #[serde(default)]
    pub follow_up: Option<FollowUp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum GatewayRequest {
    Stage(StageRequest),
    NewRepo(NewRepoRequest),
    PushPr(PushPrRequest),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageOutput {
    pub bundle_key: String,
    pub bundle_uri: String,
    pub base_sha: String,
    pub base_ref: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// The answer to the request's `containerSignals`. Its presence says the check ran; `found` names the first signal that matched and the
    /// path it matched. Absent when the request carried no list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_check: Option<ham_core::container_signals::Check>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRepoOutput {
    pub repository_arn: String,
    pub base_sha: String,
    pub default_branch: String,
    /// False when a retry found the repository (and its `main`) in place.
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPrOutput {
    pub pull_request_id: String,
    /// The PR link (`delivery.pr` `url`).
    pub url: String,
    pub revision_id: String,
    pub source_ref: String,
    pub head_sha: String,
    /// The check rule the pull request carries: `hamh-prcheck-required`, or none when the head has no root `buildspec.yml`.
    pub approval_rules: Vec<String>,
    /// The comments posted, the no-check note included: 0 on a pull request with no commits.
    pub comments: usize,
    /// Only for a follow-up run: where the pull request points and whether the old one links back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<FollowUpOpened>,
}

pub struct Gateway<C, A, B, R> {
    pub codecommit: C,
    pub allowlist: A,
    pub blobs: B,
    pub remotes: R,
    pub git: Git,
    /// The stage word (`direct`): the stage tag and the PR-check build role `hamh-<stage>-prcheck-build`.
    pub stage: String,
    /// The value of the `owner` tag of the repositories the gateway creates: the stage's owner (`HAMH_OWNER`, which the CDK app sets from the account file), the value the stack's IAM condition
    /// `aws:RequestTag/owner` requires.
    pub owner: String,
    pub bucket: String,
    /// Parent of the per-invocation scratch directories.
    pub work_root: PathBuf,
    /// Unix seconds.
    pub clock: fn() -> u64,
    /// The stage's repository-name rule (`HAMH_REPO_NAMES`; default `hamh-e2e-*`): a target must be covered by it, besides being on the allowlist.
    pub repo_names: RepoNames,
}

impl<C, A, B, R> std::fmt::Debug for Gateway<C, A, B, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("stage", &self.stage)
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, DeliveryError> {
    serde_json::to_value(v).map_err(|e| DeliveryError::unavailable(e.to_string()))
}

fn same_rule(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

impl<C: CodeCommit, A: Allowlist, B: Blobs, R: Remotes> Gateway<C, A, B, R> {
    pub async fn handle(&self, req: GatewayRequest) -> Result<Value, DeliveryError> {
        match req {
            // Boxed: the operation futures nest deep enough to overflow the layout query depth
            // of the Lambda main future.
            GatewayRequest::Stage(r) => to_value(&Box::pin(self.stage(&r)).await?),
            GatewayRequest::NewRepo(r) => to_value(&Box::pin(self.new_repo(&r)).await?),
            GatewayRequest::PushPr(r) => to_value(&Box::pin(self.push_pr(&r)).await?),
        }
    }

    /// Scheme and prefix, allowlist, then the repository's ARN; nothing reads the repository first.
    async fn resolve(&self, target: &RepoTarget) -> Result<(String, RepoMeta), DeliveryError> {
        let name = codecommit_name_in(target, &self.repo_names)?;
        check_allowlisted(target, self.allowlist.repo_allow(target).await?.as_ref())?;
        let meta = self.codecommit.get_repository(name).await?.ok_or_else(|| {
            DeliveryError::Refused(format!(
                "refused: cc:{name} does not exist in {E2E_ACCOUNT} {E2E_REGION}"
            ))
        })?;
        check_repository_arn(name, meta.arn.as_deref(), meta.account_id.as_deref())?;
        Ok((name.to_string(), meta))
    }

    pub async fn stage(&self, r: &StageRequest) -> Result<StageOutput, DeliveryError> {
        harness_ref(&r.run_id)?;
        // A bad line of the list is refused before any read, like every other check of the request.
        if let Some(list) = &r.container_signals {
            ham_core::container_signals::resolve(Some(list.as_slice()))
                .map_err(DeliveryError::invalid)?;
        }
        if let Some(from) = &r.from_run {
            return self.stage_from_run(r, from).await;
        }
        match &r.target {
            StageTarget::Repo(t)
            | StageTarget::Run(RunTarget::Repo { repo: t })
            | StageTarget::Run(RunTarget::NewRepo { repo: t }) => self.stage_repo(r, t).await,
            StageTarget::Run(RunTarget::S3Only) => self.stage_empty(r).await,
        }
    }

    /// (`ham continue`) and: the new run starts from the head an earlier run delivered. The `s3-only` target takes it with no
    /// `baseRef`; a repository target (`cc:`, allowlisted, not `--new-repo`) takes it with no `baseRef` or with `harness/<source run>`, the
    /// branch that run delivered on. Every request check comes before the first read. The source run's verified bundle is then loaded as
    /// `push-pr` loads it (it must verify and hold `from.head_sha` at `refs/heads/harness/<source>`), `refs/heads/main` is set to that head and
    /// the usual base bundle is published: the new run's base is the old head, with the full history behind it, whatever has become of the
    /// source run's branch in the repository, and that branch does not travel. A repository target is checked (allowlist, then the
    /// repository) and otherwise untouched: no branch is read from it and nothing is written to it.
    async fn stage_from_run(
        &self,
        r: &StageRequest,
        from: &FromRun,
    ) -> Result<StageOutput, DeliveryError> {
        harness_ref(&from.run_id)?;
        if from.run_id == r.run_id {
            return Err(DeliveryError::invalid(
                "fromRun names the run itself; a follow-up starts from an earlier run",
            ));
        }
        check_head_sha(&from.head_sha)?;
        let source_branch = format!("harness/{}", from.run_id);
        match &r.target {
            StageTarget::Run(RunTarget::S3Only) => {
                if r.base_ref.is_some() {
                    return Err(DeliveryError::invalid(
                        "fromRun starts an s3-only run with no baseRef (a repository run may name only its source run's branch as baseRef)",
                    ));
                }
            }
            StageTarget::Run(RunTarget::NewRepo { .. }) => {
                return Err(DeliveryError::invalid(
                    "fromRun does not start a --new-repo run: a follow-up targets the repository itself",
                ));
            }
            StageTarget::Repo(t) | StageTarget::Run(RunTarget::Repo { repo: t }) => {
                if let Some(b) = &r.base_ref
                    && normalize_base_ref(b)? != format!("refs/heads/{source_branch}")
                {
                    return Err(DeliveryError::invalid(format!(
                        "fromRun starts a repository run from the head of {source_branch}; baseRef {b} is not accepted (name {source_branch} or none)"
                    )));
                }
                // The scheme and prefix, the allowlist and the repository's ARN: the same checks as any stage, before the bucket is read.
                self.resolve(t).await?;
            }
        }
        let work = scratch(&self.work_root, "stage-")?;
        let loaded = load_verified(
            &self.git,
            &self.blobs,
            &self.bucket,
            work.path(),
            &from.run_id,
            &from.head_sha,
        )
        .await?
        .map_err(|p| DeliveryError::invalid(p.to_string()))?;
        let base_ref = format!("refs/heads/{DESTINATION_BRANCH}");
        self.git
            .update_ref(&loaded.repo, &base_ref, &from.head_sha)
            .await?;
        self.publish_base(
            &r.run_id,
            work.path(),
            &loaded.repo,
            base_ref,
            from.head_sha.clone(),
            r.container_signals.as_deref(),
        )
        .await
    }

    /// `--s3-only`: one empty-tree root commit by the harness identity at a fixed time on `main`.
    async fn stage_empty(&self, r: &StageRequest) -> Result<StageOutput, DeliveryError> {
        let base_ref = format!("refs/heads/{DESTINATION_BRANCH}");
        if let Some(b) = &r.base_ref
            && normalize_base_ref(b)? != base_ref
        {
            return Err(DeliveryError::invalid(format!(
                "an s3-only run starts from an empty {base_ref}, not baseRef {b}"
            )));
        }
        let work = scratch(&self.work_root, "stage-")?;
        let repo = work.path().join("repo.git");
        self.git.init_bare(&repo).await?;
        let message = empty_base_message(&r.run_id);
        let sha = self
            .git
            .empty_commit(&repo, &message, EMPTY_BASE_UNIX)
            .await?;
        self.git.update_ref(&repo, &base_ref, &sha).await?;
        self.publish_base(
            &r.run_id,
            work.path(),
            &repo,
            base_ref,
            sha,
            r.container_signals.as_deref(),
        )
        .await
    }

    async fn stage_repo(
        &self,
        r: &StageRequest,
        target: &RepoTarget,
    ) -> Result<StageOutput, DeliveryError> {
        let (name, meta) = self.resolve(target).await?;
        let base_ref = match &r.base_ref {
            Some(b) => normalize_base_ref(b)?,
            None => normalize_base_ref(meta.default_branch.as_deref().ok_or_else(|| {
                DeliveryError::Refused(format!(
                    "refused: cc:{name} has no default branch (an empty repository); start it with --new-repo"
                ))
            })?)?,
        };
        let work = scratch(&self.work_root, "stage-")?;
        let repo = work.path().join("repo.git");
        self.git.init_bare(&repo).await?;
        let remote = self.remotes.remote(&name).await?;
        let refspec = format!("+{base_ref}:{base_ref}");
        if let Err(e) = self.git.fetch(&repo, &remote, &[refspec.as_str()]).await {
            return Err(if e.to_string().contains("couldn't find remote ref") {
                DeliveryError::invalid(format!("baseRef {base_ref} does not exist in cc:{name}"))
            } else {
                e
            });
        }
        let base_sha =
            self.git.rev_parse(&repo, &base_ref).await?.ok_or_else(|| {
                DeliveryError::unavailable(format!("{base_ref} fetched but absent"))
            })?;
        self.publish_base(
            &r.run_id,
            work.path(),
            &repo,
            base_ref,
            base_sha,
            r.container_signals.as_deref(),
        )
        .await
    }

    /// Bundles `base_ref` with `HEAD` on it, then writes the bundle and the base tree listing (blobs
    /// only, by path), each with its SHA-256 in the metadata.
    async fn publish_base(
        &self,
        run_id: &str,
        work: &Path,
        repo: &Path,
        base_ref: String,
        base_sha: String,
        container_signals: Option<&[String]>,
    ) -> Result<StageOutput, DeliveryError> {
        self.git.symbolic_head(repo, &base_ref).await?;
        let bundle = work.join("repo.bundle");
        self.git
            .bundle_create(repo, &bundle, &["HEAD", base_ref.as_str()])
            .await?;
        self.git.bundle_verify(repo, &bundle).await?;
        let (sha256, size_bytes) = sha256_file(&bundle)?;
        // The check reads the object database before anything is uploaded, so a failed read leaves nothing behind.
        let container_check = match container_signals {
            Some(list) => Some(self.container_check(repo, &base_sha, list).await?),
            None => None,
        };
        let key = s3::repo_bundle_key(run_id);
        self.blobs
            .put_file(
                &key,
                &bundle,
                &PutMeta {
                    content_type: "application/octet-stream",
                    sha256_hex: sha256.clone(),
                },
            )
            .await?;
        Ok(StageOutput {
            bundle_uri: format!("s3://{}/{key}", self.bucket),
            bundle_key: key,
            base_sha,
            base_ref,
            size_bytes,
            sha256,
            container_check,
        })
    }

    /// The first signal of `list` that the tree of `rev` matches. The tree is read from the object database (`ls-tree`, `cat-file`), never
    /// from a checkout. The signals are taken in order and the files a signal asks for are read in batches of [`READ_BATCH`](ham_core::container_signals::READ_BATCH),
    /// nothing is kept from one batch to the next, and a signal that matches by name is decided without reading a file. A failed read is the answer:
    /// the stage fails, and a failure is never reported as "nothing found".
    async fn container_check(
        &self,
        repo: &Path,
        rev: &str,
        list: &[String],
    ) -> Result<ham_core::container_signals::Check, DeliveryError> {
        use ham_core::container_signals::{Check, READ_BATCH, holds, resolve, sorted_paths};
        let signals = resolve(Some(list)).map_err(DeliveryError::invalid)?;
        let entries = self.git.ls_tree(repo, rev).await?;
        let blobs: std::collections::BTreeMap<&str, &str> = entries
            .iter()
            .filter(|e| e.kind == "blob")
            .map(|e| (e.path.as_str(), e.sha.as_str()))
            .collect();
        let paths: Vec<String> = blobs.keys().map(|p| (*p).to_string()).collect();
        let sorted = sorted_paths(&paths);
        for signal in &signals {
            let candidates = signal.candidates(&sorted);
            let Some(text) = signal.text() else {
                if let Some(first) = candidates.first() {
                    return Ok(Check {
                        found: Some(signal.found(first)),
                    });
                }
                continue;
            };
            for batch in candidates.chunks(READ_BATCH) {
                let shas: Vec<&str> = batch
                    .iter()
                    .filter_map(|p| blobs.get(*p).copied())
                    .collect();
                let bodies = self.git.blobs(repo, &shas).await?;
                if bodies.len() != batch.len() {
                    return Err(DeliveryError::unavailable(
                        "the container check asked git for a number of files and got another",
                    ));
                }
                for (path, body) in batch.iter().zip(&bodies) {
                    if holds(body, text) {
                        return Ok(Check {
                            found: Some(signal.found(path)),
                        });
                    }
                }
            }
        }
        Ok(Check { found: None })
    }

    fn tags(&self) -> Vec<(String, String)> {
        [
            ("project", PROJECT),
            ("stage", self.stage.as_str()),
            ("owner", self.owner.as_str()),
            ("managed-by", MANAGED_BY),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    /// `CreateRepository` of exactly the named repository, then the harness's allowlist entry for
    /// it and the empty base on `main`. A repository this run did not create (another description,
    /// or a `main` that is not this run's empty base) is refused before any allowlist write,
    /// branch or push.
    pub async fn new_repo(&self, r: &NewRepoRequest) -> Result<NewRepoOutput, DeliveryError> {
        let hr = harness_ref(&r.run_id)?;
        let name = codecommit_name_in(&r.target, &self.repo_names)?;
        check_new_repo_name(name)?;
        // An existing entry must name this target (the key could hold another's body).
        if let Some(item) = self.allowlist.repo_allow(&r.target).await? {
            check_allowlisted(&r.target, Some(&item))?;
        }
        let description = new_repo_description(&r.run_id);
        let created = self
            .codecommit
            .create_repository(name, &description, &self.tags())
            .await?;
        let meta = self.codecommit.get_repository(name).await?.ok_or_else(|| {
            DeliveryError::unavailable(format!("cc:{name} was created but reads as absent"))
        })?;
        check_repository_arn(name, meta.arn.as_deref(), meta.account_id.as_deref())?;
        if meta.description.as_deref() != Some(description.as_str()) {
            return Err(DeliveryError::Refused(format!(
                "refused: cc:{name} already exists and run {} did not create it; name a new repository",
                r.run_id
            )));
        }
        let repository_arn = meta.arn.unwrap_or_default();
        let work = scratch(&self.work_root, "new-repo-")?;
        let repo = work.path().join("repo.git");
        self.git.init_bare(&repo).await?;
        let remote = self.remotes.remote(name).await?;
        let message = empty_base_message(&r.run_id);
        let existing_main = self.git.ls_remote(&repo, &remote, DESTINATION_REF).await?;
        if let Some(sha) = &existing_main {
            self.check_own_base(&repo, &remote, name, sha, &message, &r.run_id)
                .await?;
        }
        let item = RepoAllowItem {
            target: r.target.clone(),
            added_by: format!("hamh-{}-repo-gateway run {}", self.stage, r.run_id),
            added_at: rfc3339((self.clock)()),
        };
        if let Some(existing) = self.allowlist.add_repo_allow(&item).await? {
            check_allowlisted(&r.target, Some(&existing))?;
        }
        let out = |base_sha: String| NewRepoOutput {
            repository_arn: repository_arn.clone(),
            base_sha,
            default_branch: DESTINATION_BRANCH.into(),
            created: created == Created::Created,
        };
        if let Some(sha) = existing_main {
            return Ok(out(sha));
        }
        let sha = self
            .git
            .empty_commit(&repo, &message, (self.clock)())
            .await?;
        self.git.push(&repo, &remote, &sha, &hr).await?;
        let base = match self
            .codecommit
            .create_branch(name, DESTINATION_BRANCH, &sha)
            .await?
        {
            Created::Created => sha,
            Created::Existed => {
                let main = self
                    .git
                    .ls_remote(&repo, &remote, DESTINATION_REF)
                    .await?
                    .ok_or_else(|| DeliveryError::unavailable("main exists but reads as absent"))?;
                self.check_own_base(&repo, &remote, name, &main, &message, &r.run_id)
                    .await?;
                main
            }
        };
        self.codecommit
            .update_default_branch(name, DESTINATION_BRANCH)
            .await?;
        Ok(out(base))
    }

    /// `main` at `sha` must be the empty-tree root commit this run wrote.
    async fn check_own_base(
        &self,
        repo: &Path,
        remote: &Remote,
        name: &str,
        sha: &str,
        message: &str,
        run_id: &str,
    ) -> Result<(), DeliveryError> {
        let refspec = format!("+{DESTINATION_REF}:refs/hamh/main");
        self.git.fetch(repo, remote, &[refspec.as_str()]).await?;
        if !self.git.is_empty_root(repo, sha, message).await? {
            return Err(DeliveryError::Refused(format!(
                "refused: cc:{name} main is {sha}, which is not the empty base of run {run_id}"
            )));
        }
        Ok(())
    }

    pub async fn push_pr(&self, r: &PushPrRequest) -> Result<PushPrOutput, DeliveryError> {
        let hr = push_ref(&r.run_id, r.source_ref.as_deref())?;
        check_head_sha(&r.head_sha)?;
        check_pr_text(&r.title, &r.description)?;
        for c in &r.comments {
            check_comment(c)?;
        }
        let follow = r
            .follow_up
            .as_ref()
            .map(|f| CheckedFollowUp::new(r, f, &self.repo_names))
            .transpose()?;
        let (name, _) = self.resolve(&r.target).await?;
        let work = scratch(&self.work_root, "push-pr-")?;
        let loaded = load_verified(
            &self.git,
            &self.blobs,
            &self.bucket,
            work.path(),
            &r.run_id,
            &r.head_sha,
        )
        .await?
        .map_err(|p| DeliveryError::invalid(p.to_string()))?;
        // The check runs the source commit's root buildspec.yml. The committed tree of the head decides, before anything is pushed:
        // a failed lookup is an error and never "no build file" (the machine retries it).
        let has_build_file = self
            .git
            .root_entry(&loaded.repo, &r.head_sha, BUILD_FILE)
            .await?
            .as_ref()
            .is_some_and(is_build_file);
        let remote = self.remotes.remote(&name).await?;
        // A follow-up run decides where its pull request points before anything is pushed; a conflict leaves the repository untouched.
        let aim = match &follow {
            Some(f) => Some(self.aim_follow_up(f, &name, &remote, &loaded.repo).await?),
            None => None,
        };
        let destination_ref = aim
            .as_ref()
            .map_or(DESTINATION_REF, |a| a.destination_ref.as_str());
        self.git
            .push(&loaded.repo, &remote, &r.head_sha, &hr)
            .await?;
        let pr = self
            .codecommit
            .create_pull_request(&NewPullRequest {
                repository: name.clone(),
                title: r.title.clone(),
                description: r.description.clone(),
                source_ref: hr.as_str().into(),
                destination_ref: destination_ref.into(),
                client_request_token: format!("hamh-pr-{}", r.run_id),
            })
            .await?;
        if pr.source_commit != r.head_sha {
            return Err(DeliveryError::unavailable(format!(
                "pull request {} has source commit {}, not {}",
                pr.pull_request_id, pr.source_commit, r.head_sha
            )));
        }
        // Without a build file no rule is attached: nothing could satisfy it. A rule that an earlier delivery of this pull request attached
        // (an older gateway, a retry) is never taken away, so it is kept and reported.
        let checked = has_build_file || self.has_check_rule(&pr.pull_request_id).await?;
        let rules: Vec<(&str, String)> = if checked {
            vec![(PRCHECK_RULE, prcheck_rule_content(&self.stage))]
        } else {
            Vec::new()
        };
        for (rule, content) in &rules {
            let made = self
                .codecommit
                .create_approval_rule(&pr.pull_request_id, rule, content)
                .await?;
            if made == Created::Existed {
                let current = self
                    .codecommit
                    .get_pull_request(&pr.pull_request_id)
                    .await?;
                let same = current
                    .approval_rules
                    .iter()
                    .any(|(n, c)| n == rule && same_rule(c, content));
                if !same {
                    return Err(DeliveryError::Refused(format!(
                        "refused: approval rule {rule} on pull request {} differs from the harness rule",
                        pr.pull_request_id
                    )));
                }
            }
        }
        // A pull request with no commits (source and destination commit equal: a run that
        // committed nothing) takes no comment: CodeCommit refuses every one with
        // BeforeCommitIdAndAfterCommitIdAreSameException. The returned commits decide, so a retry that gets the first request's pull
        // request decides the same way.
        let has_commits = pr.source_commit != pr.destination_commit;
        let comments = if has_commits {
            &r.comments[..]
        } else {
            &[][..]
        };
        for (i, content) in comments.iter().enumerate() {
            self.post(&name, &pr, content, format!("hamh-pr-{}-c{i}", r.run_id))
                .await?;
        }
        // The note follows the tables, with a token of its own; like them it needs a pull request with commits.
        let note = usize::from(!checked && has_commits);
        if note == 1 {
            self.post(
                &name,
                &pr,
                NO_CHECK_NOTE,
                format!("hamh-pr-{}-nocheck", r.run_id),
            )
            .await?;
        }
        let url = pr_console_url(&name, &pr.pull_request_id);
        let follow_up = match (&follow, &aim) {
            (Some(f), Some(a)) => {
                if has_commits {
                    let note = target_note(
                        &f.run_id,
                        f.f.pull_request_url.as_deref(),
                        &a.destination_ref,
                        a.aim,
                        &f.f.head_sha,
                        a.landed,
                    );
                    check_comment(&note)?;
                    self.post(&name, &pr, &note, format!("hamh-pr-{}-follow-up", r.run_id))
                        .await?;
                }
                // Last: the old pull request is the only thing here that is not this run's own. A pair of commits CodeCommit refuses for good never stops the new one;
                // a transient failure fails the whole delivery, which the machine retries (every call above is idempotent).
                let linked = match &a.old_pr {
                    Some(old) => self.link_old(&name, old, &r.run_id, &url).await?,
                    None => false,
                };
                Some(FollowUpOpened {
                    destination_ref: a.destination_ref.clone(),
                    aim: a.aim,
                    old_pull_request_id: a.old_pr.as_ref().map(|p| p.pull_request_id.clone()),
                    old_pull_request_linked: linked,
                })
            }
            _ => None,
        };
        Ok(PushPrOutput {
            url,
            pull_request_id: pr.pull_request_id,
            revision_id: pr.revision_id,
            source_ref: hr.as_str().into(),
            head_sha: r.head_sha.clone(),
            approval_rules: rules.iter().map(|(n, _)| (*n).to_string()).collect(),
            comments: comments.len() + note,
            follow_up,
        })
    }

    /// Whether the pull request already carries the check rule (`GetPullRequest`).
    async fn has_check_rule(&self, pull_request_id: &str) -> Result<bool, DeliveryError> {
        let current = self.codecommit.get_pull_request(pull_request_id).await?;
        Ok(current
            .approval_rules
            .iter()
            .any(|(n, _)| n == PRCHECK_RULE))
    }

    /// One comment on the new pull request, between its two commits.
    async fn post(
        &self,
        repository: &str,
        pr: &PullRequestInfo,
        content: &str,
        token: String,
    ) -> Result<(), DeliveryError> {
        self.codecommit
            .post_comment(&NewComment {
                repository: repository.to_string(),
                pull_request_id: pr.pull_request_id.clone(),
                before_commit_id: pr.destination_commit.clone(),
                after_commit_id: pr.source_commit.clone(),
                content: content.to_string(),
                client_request_token: token,
            })
            .await
    }

    /// Where the pull request of a follow-up run points: the facts are read from the repository and from the old run's pull
    /// request, `follow_up::decide` chooses, and a conflict is a `Refused` that the run reports as "pull request not opened".
    async fn aim_follow_up(
        &self,
        f: &CheckedFollowUp<'_>,
        name: &str,
        remote: &Remote,
        repo: &Path,
    ) -> Result<Aimed, DeliveryError> {
        // The changes of the new pull request are listed since the recorded commit, so that commit must be in the history of the head being delivered.
        let recorded = self.git.rev_parse(repo, &f.f.head_sha).await?;
        let descends = match recorded {
            Some(_) => {
                self.git
                    .is_ancestor(repo, &f.f.head_sha, &f.head_sha)
                    .await?
            }
            None => false,
        };
        if !descends {
            return Err(DeliveryError::invalid(format!(
                "followUp.headSha {} is not in the history of the head {} being delivered",
                short_sha(&f.f.head_sha),
                short_sha(&f.head_sha)
            )));
        }
        let old_pr = match &f.old_pr_id {
            Some(id) => {
                let info = self.codecommit.get_pull_request(id).await?;
                // `GetPullRequest` takes only an id, and ids are shared by every repository of the account.
                if info.repository != name || info.source_ref != f.old_ref.as_str() {
                    return Err(DeliveryError::invalid(format!(
                        "pull request {id} is not the pull request of run {} in cc:{name} (it is {} of {})",
                        f.run_id, info.source_ref, info.repository
                    )));
                }
                Some(info)
            }
            None => None,
        };
        let destination_ref = match &old_pr {
            Some(p) => {
                // A destination read back from AWS is a branch of this repository before it is used as a ref.
                let is_branch = p
                    .destination_ref
                    .strip_prefix("refs/heads/")
                    .and_then(|b| normalize_base_ref(b).ok())
                    .is_some_and(|n| n == p.destination_ref);
                if !is_branch {
                    return Err(DeliveryError::invalid(format!(
                        "pull request {} has the destination {:?}, which is not a branch",
                        p.pull_request_id, p.destination_ref
                    )));
                }
                p.destination_ref.clone()
            }
            None => DESTINATION_REF.to_string(),
        };
        let old_tip = self.git.ls_remote(repo, remote, f.old_ref.as_str()).await?;
        let destination_tip = self.git.ls_remote(repo, remote, &destination_ref).await?;
        // Work that reached the destination by git alone (a merge outside the pull request) counts as merged as well.
        let merged_in_git = match &destination_tip {
            Some(tip) => {
                let refspec = format!("+{destination_ref}:refs/hamh/follow-up-destination");
                self.git.fetch(repo, remote, &[refspec.as_str()]).await?;
                self.git.rev_parse(repo, &f.f.head_sha).await?.is_some()
                    && self.git.is_ancestor(repo, &f.f.head_sha, tip).await?
            }
            None => false,
        };
        let merged = merged_in_git || old_pr.as_ref().is_some_and(|p| p.merged);
        let plan = decide(&Facts {
            old_run: &f.run_id,
            recorded_head: &f.f.head_sha,
            old_branch_tip: old_tip.as_deref(),
            destination_ref: &destination_ref,
            destination_tip: destination_tip.as_deref(),
            merged,
        });
        match plan {
            Plan::Open {
                destination_ref,
                aim,
            } => Ok(Aimed {
                destination_ref,
                aim,
                landed: Landed {
                    pr_merged: old_pr.as_ref().is_some_and(|p| p.merged),
                    in_history: merged_in_git,
                },
                old_pr,
            }),
            Plan::Conflict(message) => Err(DeliveryError::Refused(message)),
        }
    }

    /// A comment on the old pull request that links to the new one. CodeCommit takes a comment only between two different commits, so the
    /// pair is the destination commit and the source commit, else the merge base and the source commit; a pair CodeCommit refuses for good
    /// (`Invalid`) is followed by the next, and `false` means no pair took it. A transient failure is returned, so the whole delivery is retried.
    async fn link_old(
        &self,
        name: &str,
        old: &PullRequestInfo,
        new_run: &str,
        new_url: &str,
    ) -> Result<bool, DeliveryError> {
        let content = link_note(new_run, new_url);
        check_comment(&content)?;
        // CreatePullRequest's destination commit is the pair the harness has proven on new pull requests; the merge base is the fallback (a merged
        // pull request can report a destination commit equal to its source commit). Each pair has its own token, so a pair CodeCommit takes is
        // asked again with the same arguments on a retry and one it refused for good does not poison the next.
        for (before, token) in [
            (&old.destination_commit, format!("hamh-pr-{new_run}-link")),
            (&old.merge_base, format!("hamh-pr-{new_run}-link-mb")),
        ] {
            if before.is_empty() || old.source_commit.is_empty() || *before == old.source_commit {
                continue;
            }
            let sent = self
                .codecommit
                .post_comment(&NewComment {
                    repository: name.to_string(),
                    pull_request_id: old.pull_request_id.clone(),
                    before_commit_id: before.clone(),
                    after_commit_id: old.source_commit.clone(),
                    content: content.clone(),
                    client_request_token: token,
                })
                .await;
            match sent {
                Ok(()) => return Ok(true),
                Err(DeliveryError::Invalid(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// A follow-up member whose own fields are right, before anything is read.
struct CheckedFollowUp<'a> {
    f: &'a FollowUp,
    run_id: String,
    /// The head this run delivers.
    head_sha: String,
    old_ref: HarnessRef,
    old_pr_id: Option<String>,
}

impl<'a> CheckedFollowUp<'a> {
    fn new(r: &PushPrRequest, f: &'a FollowUp, names: &RepoNames) -> Result<Self, DeliveryError> {
        let old_ref = harness_ref(&f.run_id)?;
        if f.run_id == r.run_id {
            return Err(DeliveryError::invalid(
                "followUp names the run itself; a follow-up continues an earlier run",
            ));
        }
        check_head_sha(&f.head_sha)?;
        let old_pr_id = match &f.pull_request_url {
            None => None,
            Some(url) => {
                let name = codecommit_name_in(&r.target, names)?;
                Some(pull_request_id_in(url, name).ok_or_else(|| {
                    DeliveryError::invalid(format!(
                        "followUp.pullRequestUrl is not the link of a pull request in cc:{name}"
                    ))
                })?)
            }
        };
        Ok(Self {
            f,
            run_id: f.run_id.clone(),
            head_sha: r.head_sha.clone(),
            old_ref,
            old_pr_id,
        })
    }
}

/// What `aim_follow_up` decided.
struct Aimed {
    destination_ref: String,
    aim: Aim,
    old_pr: Option<PullRequestInfo>,
    landed: Landed,
}
