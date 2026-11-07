//! CodeCommit, S3 and DynamoDB implementations of the delivery traits, and the signed git remote.
//! Items are the harness-protocol serde types plus `PK`/`SK` from their key builders.

use crate::credential::{AwsKeys, git_credential, repo_url};
use crate::error::DeliveryError;
use crate::gateway::{
    CodeCommit, Created, NewComment, NewPullRequest, PullRequestInfo, Remotes, RepoMeta,
};
use crate::git::Remote;
use crate::ports::{Allowlist, Artifacts, Blobs, PutMeta};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sdk_codecommit::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_codecommit::operation::create_branch::CreateBranchError;
use aws_sdk_codecommit::operation::create_pull_request::CreatePullRequestError;
use aws_sdk_codecommit::operation::create_pull_request_approval_rule::CreatePullRequestApprovalRuleError;
use aws_sdk_codecommit::operation::create_repository::CreateRepositoryError;
use aws_sdk_codecommit::operation::get_pull_request::GetPullRequestError;
use aws_sdk_codecommit::operation::get_repository::GetRepositoryError;
use aws_sdk_codecommit::operation::post_comment_for_pull_request::PostCommentForPullRequestError;
use aws_sdk_codecommit::operation::update_default_branch::UpdateDefaultBranchError;
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::types::AttributeValue;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use harness_protocol::store::{ArtifactItem, Keyed, RepoAllowItem, keys};
use harness_protocol::target::RepoTarget;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;

fn unavailable<E: std::error::Error>(e: E) -> DeliveryError {
    DeliveryError::Unavailable(DisplayErrorContext(e).to_string())
}

/// An S3 failure: `AccessDenied` (the role has no grant for the key or the action, or the key is missing and the role
/// cannot list: the same answer on every retry) is its own error type, which the run machine catches without a retry
/// (retrying it only delays the end of the run). Every other S3 failure stays `Unavailable`.
/// CodeCommit access errors are not mapped here: right after a push they can be a propagation race and are retried.
fn s3_error<E, R>(what: &str, e: SdkError<E, R>) -> DeliveryError
where
    E: std::error::Error + ProvideErrorMetadata + 'static,
    R: std::fmt::Debug,
{
    if e.as_service_error()
        .is_some_and(|s| s.code() == Some("AccessDenied"))
    {
        return DeliveryError::AccessDenied(format!("{what}: {}", DisplayErrorContext(e)));
    }
    unavailable(e)
}

/// A CodeCommit answer no retry can change is `Invalid`, which the run machine catches without a
/// retry (an HTTP 400 that is retried five times is refused five times): the refusal of a
/// comment on a pull request with no commits, and the input-validation `Invalid*` exceptions,
/// matched on the SDK's typed variants. Everything else stays `Unavailable` and is retried:
/// throttling, 5xx and transport errors, access and key errors, and the `...DoesNotExist` answers,
/// which right after a push can name a reference the API does not see yet.
fn codecommit_error<E, R>(op: &str, e: SdkError<E, R>, permanent: fn(&E) -> bool) -> DeliveryError
where
    E: std::error::Error + ProvideErrorMetadata + 'static,
    R: std::fmt::Debug,
{
    match e.as_service_error() {
        Some(s) if permanent(s) => DeliveryError::Invalid(format!(
            "CodeCommit {op}: {}: {}",
            s.code().unwrap_or("unknown"),
            s.message().unwrap_or_default()
        )),
        _ => unavailable(e),
    }
}

fn get_repository_permanent(e: &GetRepositoryError) -> bool {
    matches!(e, GetRepositoryError::InvalidRepositoryNameException(_))
}

fn create_repository_permanent(e: &CreateRepositoryError) -> bool {
    use CreateRepositoryError as E;
    matches!(
        e,
        E::InvalidRepositoryDescriptionException(_)
            | E::InvalidRepositoryNameException(_)
            | E::InvalidSystemTagUsageException(_)
            | E::InvalidTagsMapException(_)
    )
}

fn create_branch_permanent(e: &CreateBranchError) -> bool {
    use CreateBranchError as E;
    matches!(
        e,
        E::InvalidBranchNameException(_)
            | E::InvalidCommitIdException(_)
            | E::InvalidRepositoryNameException(_)
    )
}

fn update_default_branch_permanent(e: &UpdateDefaultBranchError) -> bool {
    use UpdateDefaultBranchError as E;
    matches!(
        e,
        E::InvalidBranchNameException(_) | E::InvalidRepositoryNameException(_)
    )
}

fn create_pull_request_permanent(e: &CreatePullRequestError) -> bool {
    use CreatePullRequestError as E;
    matches!(
        e,
        E::InvalidClientRequestTokenException(_)
            | E::InvalidDescriptionException(_)
            | E::InvalidReferenceNameException(_)
            | E::InvalidRepositoryNameException(_)
            | E::InvalidTargetException(_)
            | E::InvalidTargetsException(_)
            | E::InvalidTitleException(_)
            // A follow-up whose old branch moved between two attempts repeats a token with a changed request: no retry changes that.
            | E::IdempotencyParameterMismatchException(_)
            | E::SourceAndDestinationAreSameException(_)
    )
}

fn get_pull_request_permanent(e: &GetPullRequestError) -> bool {
    matches!(e, GetPullRequestError::InvalidPullRequestIdException(_))
}

fn create_approval_rule_permanent(e: &CreatePullRequestApprovalRuleError) -> bool {
    use CreatePullRequestApprovalRuleError as E;
    matches!(
        e,
        E::InvalidApprovalRuleContentException(_)
            | E::InvalidApprovalRuleNameException(_)
            | E::InvalidPullRequestIdException(_)
    )
}

fn post_comment_permanent(e: &PostCommentForPullRequestError) -> bool {
    use PostCommentForPullRequestError as E;
    matches!(
        e,
        E::BeforeCommitIdAndAfterCommitIdAreSameException(_)
            | E::InvalidClientRequestTokenException(_)
            | E::InvalidCommitIdException(_)
            | E::InvalidFileLocationException(_)
            | E::InvalidFilePositionException(_)
            | E::InvalidPathException(_)
            | E::InvalidPullRequestIdException(_)
            | E::InvalidRelativeFileVersionEnumException(_)
            | E::InvalidRepositoryNameException(_)
            | E::IdempotencyParameterMismatchException(_)
            | E::RepositoryNotAssociatedWithPullRequestException(_)
    )
}

#[derive(Clone, Debug)]
pub struct CodeCommitApi {
    pub client: aws_sdk_codecommit::Client,
}

fn pull_request_info(
    pr: Option<&aws_sdk_codecommit::types::PullRequest>,
) -> Result<PullRequestInfo, DeliveryError> {
    let pr = pr.ok_or_else(|| DeliveryError::unavailable("CodeCommit returned no pull request"))?;
    let target = pr.pull_request_targets().first();
    Ok(PullRequestInfo {
        pull_request_id: pr.pull_request_id().unwrap_or_default().to_string(),
        revision_id: pr.revision_id().unwrap_or_default().to_string(),
        source_commit: target
            .and_then(|t| t.source_commit())
            .unwrap_or_default()
            .to_string(),
        destination_commit: target
            .and_then(|t| t.destination_commit())
            .unwrap_or_default()
            .to_string(),
        repository: target
            .and_then(|t| t.repository_name())
            .unwrap_or_default()
            .to_string(),
        source_ref: target
            .and_then(|t| t.source_reference())
            .unwrap_or_default()
            .to_string(),
        destination_ref: target
            .and_then(|t| t.destination_reference())
            .unwrap_or_default()
            .to_string(),
        merge_base: target
            .and_then(|t| t.merge_base())
            .unwrap_or_default()
            .to_string(),
        merged: target
            .and_then(|t| t.merge_metadata())
            .is_some_and(|m| m.is_merged()),
        approval_rules: pr
            .approval_rules()
            .iter()
            .map(|r| {
                (
                    r.approval_rule_name().unwrap_or_default().to_string(),
                    r.approval_rule_content().unwrap_or_default().to_string(),
                )
            })
            .collect(),
    })
}

impl CodeCommit for CodeCommitApi {
    async fn get_repository(&self, name: &str) -> Result<Option<RepoMeta>, DeliveryError> {
        match self
            .client
            .get_repository()
            .repository_name(name)
            .send()
            .await
        {
            Ok(out) => Ok(out.repository_metadata().map(|m| RepoMeta {
                name: m.repository_name().unwrap_or(name).to_string(),
                arn: m.arn().map(str::to_string),
                account_id: m.account_id().map(str::to_string),
                default_branch: m.default_branch().map(str::to_string),
                description: m.repository_description().map(str::to_string),
            })),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_repository_does_not_exist_exception()) =>
            {
                Ok(None)
            }
            Err(e) => Err(codecommit_error(
                "GetRepository",
                e,
                get_repository_permanent,
            )),
        }
    }

    async fn create_repository(
        &self,
        name: &str,
        description: &str,
        tags: &[(String, String)],
    ) -> Result<Created, DeliveryError> {
        let tags: HashMap<String, String> = tags.iter().cloned().collect();
        let res = self
            .client
            .create_repository()
            .repository_name(name)
            .repository_description(description)
            .set_tags(Some(tags))
            .send()
            .await;
        match res {
            Ok(_) => Ok(Created::Created),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_repository_name_exists_exception()) =>
            {
                Ok(Created::Existed)
            }
            Err(e) => Err(codecommit_error(
                "CreateRepository",
                e,
                create_repository_permanent,
            )),
        }
    }

    async fn create_branch(
        &self,
        repository: &str,
        branch: &str,
        commit_id: &str,
    ) -> Result<Created, DeliveryError> {
        let res = self
            .client
            .create_branch()
            .repository_name(repository)
            .branch_name(branch)
            .commit_id(commit_id)
            .send()
            .await;
        match res {
            Ok(_) => Ok(Created::Created),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_branch_name_exists_exception()) =>
            {
                Ok(Created::Existed)
            }
            Err(e) => Err(codecommit_error("CreateBranch", e, create_branch_permanent)),
        }
    }

    async fn update_default_branch(
        &self,
        repository: &str,
        branch: &str,
    ) -> Result<(), DeliveryError> {
        self.client
            .update_default_branch()
            .repository_name(repository)
            .default_branch_name(branch)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| {
                codecommit_error("UpdateDefaultBranch", e, update_default_branch_permanent)
            })
    }

    async fn create_pull_request(
        &self,
        pr: &NewPullRequest,
    ) -> Result<PullRequestInfo, DeliveryError> {
        let target = aws_sdk_codecommit::types::Target::builder()
            .repository_name(&pr.repository)
            .source_reference(&pr.source_ref)
            .destination_reference(&pr.destination_ref)
            .build()
            .map_err(unavailable)?;
        let out = self
            .client
            .create_pull_request()
            .title(&pr.title)
            .description(&pr.description)
            .targets(target)
            .client_request_token(&pr.client_request_token)
            .send()
            .await
            .map_err(|e| codecommit_error("CreatePullRequest", e, create_pull_request_permanent))?;
        pull_request_info(out.pull_request())
    }

    async fn get_pull_request(
        &self,
        pull_request_id: &str,
    ) -> Result<PullRequestInfo, DeliveryError> {
        let out = self
            .client
            .get_pull_request()
            .pull_request_id(pull_request_id)
            .send()
            .await
            .map_err(|e| codecommit_error("GetPullRequest", e, get_pull_request_permanent))?;
        pull_request_info(out.pull_request())
    }

    async fn create_approval_rule(
        &self,
        pull_request_id: &str,
        name: &str,
        content: &str,
    ) -> Result<Created, DeliveryError> {
        let res = self
            .client
            .create_pull_request_approval_rule()
            .pull_request_id(pull_request_id)
            .approval_rule_name(name)
            .approval_rule_content(content)
            .send()
            .await;
        match res {
            Ok(_) => Ok(Created::Created),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_approval_rule_name_already_exists_exception()) =>
            {
                Ok(Created::Existed)
            }
            Err(e) => Err(codecommit_error(
                "CreatePullRequestApprovalRule",
                e,
                create_approval_rule_permanent,
            )),
        }
    }

    async fn post_comment(&self, c: &NewComment) -> Result<(), DeliveryError> {
        self.client
            .post_comment_for_pull_request()
            .pull_request_id(&c.pull_request_id)
            .repository_name(&c.repository)
            .before_commit_id(&c.before_commit_id)
            .after_commit_id(&c.after_commit_id)
            .content(&c.content)
            .client_request_token(&c.client_request_token)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| codecommit_error("PostCommentForPullRequest", e, post_comment_permanent))
    }
}

/// The CodeCommit HTTPS remote, signed from the Lambda role's current credentials right before
/// each git call.
#[derive(Clone, Debug)]
pub struct CodeCommitRemotes {
    pub credentials: SharedCredentialsProvider,
    pub region: String,
    pub clock: fn() -> u64,
}

impl Remotes for CodeCommitRemotes {
    async fn remote(&self, repository: &str) -> Result<Remote, DeliveryError> {
        let c = self
            .credentials
            .provide_credentials()
            .await
            .map_err(|e| DeliveryError::unavailable(format!("role credentials: {e}")))?;
        let keys = AwsKeys {
            access_key_id: c.access_key_id().to_string(),
            secret_access_key: c.secret_access_key().to_string(),
            session_token: c.session_token().map(str::to_string),
        };
        Ok(Remote {
            url: repo_url(&self.region, repository),
            credential: Some(git_credential(
                &keys,
                &self.region,
                repository,
                (self.clock)(),
            )),
        })
    }
}

#[derive(Clone, Debug)]
pub struct S3Blobs {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

fn checksum_b64(sha256_hex: &str) -> Result<String, DeliveryError> {
    let bytes = (0..sha256_hex.len() / 2)
        .map(|i| u8::from_str_radix(&sha256_hex[2 * i..2 * i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| DeliveryError::invalid(format!("{sha256_hex:?} is not a hex SHA-256")))?;
    if bytes.len() != 32 {
        return Err(DeliveryError::invalid(format!(
            "{sha256_hex:?} is not a hex SHA-256"
        )));
    }
    Ok(STANDARD.encode(bytes))
}

impl Blobs for S3Blobs {
    async fn get_to_file(&self, key: &str, path: &Path) -> Result<Option<u64>, DeliveryError> {
        let res = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        let mut out = match res {
            Ok(o) => o,
            Err(e) if e.as_service_error().is_some_and(|s| s.is_no_such_key()) => return Ok(None),
            Err(e) => return Err(s3_error(&format!("s3 get {key}"), e)),
        };
        let mut file = tokio::fs::File::create(path).await?;
        let mut size = 0u64;
        while let Some(chunk) = out
            .body
            .try_next()
            .await
            .map_err(|e| DeliveryError::unavailable(format!("s3 get {key}: {e}")))?
        {
            file.write_all(&chunk).await?;
            size += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(Some(size))
    }

    /// Bucket-default encryption (SSE-KMS with the data key); the SHA-256 goes into the metadata
    /// and the S3 checksum, which S3 verifies on receipt.
    async fn put_file(&self, key: &str, path: &Path, meta: &PutMeta) -> Result<(), DeliveryError> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .map_err(|e| DeliveryError::unavailable(format!("read {}: {e}", path.display())))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(meta.content_type)
            .metadata("sha256", &meta.sha256_hex)
            .checksum_sha256(checksum_b64(&meta.sha256_hex)?)
            .body(body)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| s3_error(&format!("s3 put {key}"), e))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, DeliveryError> {
        let mut keys = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| s3_error(&format!("s3 list {prefix}"), e))?;
            keys.extend(
                page.contents()
                    .iter()
                    .filter_map(|o| o.key().map(str::to_string)),
            );
        }
        Ok(keys)
    }
}

#[derive(Clone, Debug)]
pub struct DynamoTable {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

type Item = HashMap<String, AttributeValue>;

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}

fn item_of<T: Serialize + Keyed>(t: &T) -> Result<Item, DeliveryError> {
    let key = t.key();
    let mut item: Item =
        serde_dynamo::to_item(t).map_err(|e| DeliveryError::unavailable(e.to_string()))?;
    item.insert("PK".into(), s(key.pk));
    item.insert("SK".into(), s(key.sk));
    Ok(item)
}

impl DynamoTable {
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        pk: String,
        sk: String,
    ) -> Result<Option<T>, DeliveryError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(pk))
            .key("SK", s(sk))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        match out.item {
            Some(item) => serde_dynamo::from_item(item)
                .map(Some)
                .map_err(|e| DeliveryError::unavailable(e.to_string())),
            None => Ok(None),
        }
    }
}

impl Allowlist for DynamoTable {
    async fn repo_allow(
        &self,
        target: &RepoTarget,
    ) -> Result<Option<RepoAllowItem>, DeliveryError> {
        self.get(keys::repo_allow_pk(target), keys::META.into())
            .await
    }

    async fn add_repo_allow(
        &self,
        item: &RepoAllowItem,
    ) -> Result<Option<RepoAllowItem>, DeliveryError> {
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item_of(item)?))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        let existing = match res {
            Ok(_) => None,
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                let stored: Option<RepoAllowItem> = self
                    .get(keys::repo_allow_pk(&item.target), keys::META.into())
                    .await?;
                Some(stored.ok_or_else(|| {
                    DeliveryError::unavailable(format!(
                        "{} failed its condition but reads as absent",
                        item.target.allowlist_key()
                    ))
                })?)
            }
            Err(e) => return Err(unavailable(e)),
        };
        // The listing `ham doctor` reads (idempotent; skipped for a key naming another target).
        if existing.as_ref().is_none_or(|x| x.target == item.target) {
            self.client
                .update_item()
                .table_name(&self.table)
                .key("PK", s(keys::REPO_ALLOW_INDEX_PK))
                .key("SK", s(keys::META))
                .update_expression("ADD targets :t")
                .expression_attribute_values(
                    ":t",
                    AttributeValue::Ss(vec![item.target.to_string()]),
                )
                .send()
                .await
                .map_err(unavailable)?;
        }
        Ok(existing)
    }
}

impl Artifacts for DynamoTable {
    async fn put_artifact(&self, item: &ArtifactItem) -> Result<(), DeliveryError> {
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item_of(item)?))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                let key = item.key();
                let stored: Option<ArtifactItem> = self.get(key.pk.clone(), key.sk.clone()).await?;
                if stored.as_ref() == Some(item) {
                    Ok(())
                } else {
                    Err(DeliveryError::unavailable(format!(
                        "{} / {} holds another artifact",
                        key.pk, key.sk
                    )))
                }
            }
            Err(e) => Err(unavailable(e)),
        }
    }
}
