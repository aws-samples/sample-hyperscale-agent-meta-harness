//! CodeCommit, S3 and DynamoDB adapters and the signed git remote, against recorded HTTP exchanges
//! (aws-smithy StaticReplayClient, as in crates/control-api/tests/aws_adapters.rs): the exact
//! requests they send and how they read the responses and the service errors. No network.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use base64::Engine as _;
use delivery::aws::{CodeCommitApi, CodeCommitRemotes, DynamoTable, S3Blobs};
use delivery::credential::{AwsKeys, git_credential};
use delivery::error::DeliveryError;
use delivery::gateway::{CodeCommit, Created, NewComment, NewPullRequest, Remotes};
use delivery::ports::{Allowlist, Artifacts, Blobs, PutMeta};
use harness_protocol::store::{ArtifactItem, ArtifactKind, RepoAllowItem};
use harness_protocol::target::E2E_REGION;
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const REPO: &str = "hamh-e2e-python-calc-20260925";
const ARN: &str = "arn:aws:codecommit:us-west-2:800000000022:hamh-e2e-python-calc-20260925";

fn replay(responses: Vec<(u16, &str, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, content_type, body)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .header("content-type", content_type)
                        .body(SdkBody::from(body.to_string()))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

macro_rules! client {
    ($sdk:ident, $replay:expr) => {
        $sdk::Client::from_conf(
            $sdk::Config::builder()
                .behavior_version_latest()
                .region($sdk::config::Region::new("us-west-2"))
                .credentials_provider($sdk::config::Credentials::for_tests())
                .retry_config($sdk::config::retry::RetryConfig::disabled())
                .http_client($replay.clone())
                .build(),
        )
    };
}

const J11: &str = "application/x-amz-json-1.1";
const J10: &str = "application/x-amz-json-1.0";

fn cc(r: &StaticReplayClient) -> CodeCommitApi {
    CodeCommitApi {
        client: client!(aws_sdk_codecommit, r),
    }
}

fn calls(r: &StaticReplayClient) -> Vec<(String, Value)> {
    r.actual_requests()
        .map(|q| {
            let target = q
                .headers()
                .get("x-amz-target")
                .unwrap_or_default()
                .to_string();
            let body =
                serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap_or(Value::Null);
            (target, body)
        })
        .collect()
}

fn service_error(kind: &str) -> String {
    json!({"__type": kind, "message": kind}).to_string()
}

// ---- CodeCommit ---------------------------------------------------------------------------

#[tokio::test]
async fn get_repository_reads_the_arn_account_and_default_branch() {
    let body = json!({"repositoryMetadata": {
        "accountId": "800000000022", "repositoryName": REPO, "Arn": ARN, "defaultBranch": "main",
        "repositoryDescription": "ham --new-repo for run R",
        "cloneUrlHttp": format!("https://git-codecommit.us-west-2.amazonaws.com/v1/repos/{REPO}")
    }})
    .to_string();
    let r = replay(vec![(200, J11, &body)]);
    let meta = cc(&r).get_repository(REPO).await.unwrap().unwrap();
    assert_eq!(meta.name, REPO);
    assert_eq!(meta.arn.as_deref(), Some(ARN));
    assert_eq!(meta.account_id.as_deref(), Some("800000000022"));
    assert_eq!(meta.default_branch.as_deref(), Some("main"));
    assert_eq!(
        meta.description.as_deref(),
        Some("ham --new-repo for run R")
    );
    assert_eq!(
        calls(&r),
        vec![(
            "CodeCommit_20150413.GetRepository".to_string(),
            json!({"repositoryName": REPO})
        )]
    );
}

#[tokio::test]
async fn a_missing_repository_is_none_and_other_errors_are_unavailable() {
    let r = replay(vec![
        (400, J11, &service_error("RepositoryDoesNotExistException")),
        (
            400,
            J11,
            &service_error("EncryptionKeyAccessDeniedException"),
        ),
    ]);
    assert_eq!(cc(&r).get_repository(REPO).await.unwrap(), None);
    assert!(matches!(
        cc(&r).get_repository(REPO).await,
        Err(DeliveryError::Unavailable(_))
    ));
}

#[tokio::test]
async fn create_repository_sends_the_tags_in_the_request_and_an_existing_name_is_existed() {
    let r = replay(vec![
        (
            200,
            J11,
            &json!({"repositoryMetadata": {"repositoryName": "hamh-e2e-new-20260925"}}).to_string(),
        ),
        (400, J11, &service_error("RepositoryNameExistsException")),
    ]);
    let tags: Vec<(String, String)> = [
        ("project", "hyperscale-agent-meta-harness"),
        ("stage", "live"),
        ("owner", "team-a"),
        ("managed-by", "repo-gateway"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let api = cc(&r);
    assert_eq!(
        api.create_repository("hamh-e2e-new-20260925", "ham --new-repo for run R", &tags)
            .await
            .unwrap(),
        Created::Created
    );
    assert_eq!(
        api.create_repository("hamh-e2e-new-20260925", "d", &tags)
            .await
            .unwrap(),
        Created::Existed
    );
    let (target, body) = &calls(&r)[0];
    assert_eq!(target, "CodeCommit_20150413.CreateRepository");
    assert_eq!(
        body,
        &json!({"repositoryName": "hamh-e2e-new-20260925", "repositoryDescription": "ham --new-repo for run R",
                "tags": {"project": "hyperscale-agent-meta-harness", "stage": "live", "owner": "team-a", "managed-by": "repo-gateway"}})
    );
}

#[tokio::test]
async fn create_branch_and_update_default_branch_send_the_names() {
    let r = replay(vec![
        (200, J11, "{}"),
        (400, J11, &service_error("BranchNameExistsException")),
        (200, J11, "{}"),
    ]);
    let api = cc(&r);
    let sha = "a".repeat(40);
    assert_eq!(
        api.create_branch(REPO, "main", &sha).await.unwrap(),
        Created::Created
    );
    assert_eq!(
        api.create_branch(REPO, "main", &sha).await.unwrap(),
        Created::Existed
    );
    api.update_default_branch(REPO, "main").await.unwrap();
    let c = calls(&r);
    assert_eq!(
        c[0],
        (
            "CodeCommit_20150413.CreateBranch".into(),
            json!({"repositoryName": REPO, "branchName": "main", "commitId": sha})
        )
    );
    assert_eq!(
        c[2],
        (
            "CodeCommit_20150413.UpdateDefaultBranch".into(),
            json!({"repositoryName": REPO, "defaultBranchName": "main"})
        )
    );
}

fn pr_body(rules: Value) -> String {
    json!({"pullRequest": {
        "pullRequestId": "7", "title": "t", "revisionId": "rev-abc", "pullRequestStatus": "OPEN",
        "authorArn": "arn:aws:sts::800000000022:assumed-role/hamh-live-repo-gateway/x",
        "pullRequestTargets": [{"repositoryName": REPO, "sourceReference": format!("refs/heads/harness/{RUN}"),
            "destinationReference": "refs/heads/main", "sourceCommit": "s".repeat(40), "destinationCommit": "d".repeat(40)}],
        "approvalRules": rules
    }})
    .to_string()
}

#[tokio::test]
async fn create_pull_request_sends_one_target_and_the_token_and_reads_the_commits() {
    let r = replay(vec![(200, J11, &pr_body(json!([])))]);
    let info = cc(&r)
        .create_pull_request(&NewPullRequest {
            repository: REPO.into(),
            title: "Add percent_change".into(),
            description: "R1 G0".into(),
            source_ref: format!("refs/heads/harness/{RUN}"),
            destination_ref: "refs/heads/main".into(),
            client_request_token: format!("hamh-pr-{RUN}"),
        })
        .await
        .unwrap();
    assert_eq!(info.pull_request_id, "7");
    assert_eq!(info.revision_id, "rev-abc");
    assert_eq!(info.source_commit, "s".repeat(40));
    assert_eq!(info.destination_commit, "d".repeat(40));
    assert_eq!(
        calls(&r)[0],
        (
            "CodeCommit_20150413.CreatePullRequest".into(),
            json!({"title": "Add percent_change", "description": "R1 G0", "clientRequestToken": format!("hamh-pr-{RUN}"),
                   "targets": [{"repositoryName": REPO, "sourceReference": format!("refs/heads/harness/{RUN}"), "destinationReference": "refs/heads/main"}]})
        )
    );
}

#[tokio::test]
async fn approval_rules_are_created_and_read_back() {
    let rule = r#"{"Version":"2018-11-08"}"#;
    let r = replay(vec![
        (200, J11, &json!({"approvalRule": {"approvalRuleName": "hamh-prcheck-required", "approvalRuleContent": rule}}).to_string()),
        (400, J11, &service_error("ApprovalRuleNameAlreadyExistsException")),
        (200, J11, &pr_body(json!([{"approvalRuleName": "hamh-prcheck-required", "approvalRuleContent": rule}]))),
    ]);
    let api = cc(&r);
    assert_eq!(
        api.create_approval_rule("7", "hamh-prcheck-required", rule)
            .await
            .unwrap(),
        Created::Created
    );
    assert_eq!(
        api.create_approval_rule("7", "hamh-prcheck-required", rule)
            .await
            .unwrap(),
        Created::Existed
    );
    let info = api.get_pull_request("7").await.unwrap();
    assert_eq!(
        info.approval_rules,
        vec![("hamh-prcheck-required".to_string(), rule.to_string())]
    );
    let c = calls(&r);
    assert_eq!(
        c[0],
        (
            "CodeCommit_20150413.CreatePullRequestApprovalRule".into(),
            json!({"pullRequestId": "7", "approvalRuleName": "hamh-prcheck-required", "approvalRuleContent": rule})
        )
    );
    assert_eq!(
        c[2],
        (
            "CodeCommit_20150413.GetPullRequest".into(),
            json!({"pullRequestId": "7"})
        )
    );
}

#[tokio::test]
async fn a_comment_names_both_commits_and_carries_its_token() {
    let r = replay(vec![(200, J11, "{}")]);
    cc(&r)
        .post_comment(&NewComment {
            repository: REPO.into(),
            pull_request_id: "7".into(),
            before_commit_id: "d".repeat(40),
            after_commit_id: "s".repeat(40),
            content: "| R-ID |".into(),
            client_request_token: format!("hamh-pr-{RUN}-c0"),
        })
        .await
        .unwrap();
    assert_eq!(
        calls(&r)[0],
        (
            "CodeCommit_20150413.PostCommentForPullRequest".into(),
            json!({"pullRequestId": "7", "repositoryName": REPO, "beforeCommitId": "d".repeat(40), "afterCommitId": "s".repeat(40),
                "content": "| R-ID |", "clientRequestToken": format!("hamh-pr-{RUN}-c0")})
        )
    );
}

#[tokio::test]
async fn an_access_denied_push_side_call_is_unavailable_with_the_service_message() {
    let r = replay(vec![(400, J11, &json!({"__type": "AccessDeniedException", "message": "not authorized to perform codecommit:CreatePullRequest"}).to_string())]);
    let err = cc(&r)
        .create_pull_request(&NewPullRequest {
            repository: REPO.into(),
            title: "t".into(),
            description: "d".into(),
            source_ref: "refs/heads/harness/x".into(),
            destination_ref: "refs/heads/main".into(),
            client_request_token: "t".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DeliveryError::Unavailable(m) if m.contains("AccessDenied")),
        "{err}"
    );
}

fn a_comment(before: &str, after: &str) -> NewComment {
    NewComment {
        repository: REPO.into(),
        pull_request_id: "7".into(),
        before_commit_id: before.into(),
        after_commit_id: after.into(),
        content: "| R-ID |".into(),
        client_request_token: format!("hamh-pr-{RUN}-c0"),
    }
}

fn a_pull_request() -> NewPullRequest {
    NewPullRequest {
        repository: REPO.into(),
        title: "t".into(),
        description: "d".into(),
        source_ref: format!("refs/heads/harness/{RUN}"),
        destination_ref: "refs/heads/main".into(),
        client_request_token: format!("hamh-pr-{RUN}"),
    }
}

/// `Invalid` (the machine blocks at once) or `Unavailable` (the machine retries), with the
/// service's exception name in the message.
fn kind_of(kind: &str, r: Result<impl std::fmt::Debug, DeliveryError>) -> &'static str {
    let e = r.unwrap_err();
    assert!(e.to_string().contains(kind), "{kind}: {e}");
    match e {
        DeliveryError::Invalid(_) => "Invalid",
        DeliveryError::Unavailable(_) => "Unavailable",
        DeliveryError::Refused(_) => "Refused",
        DeliveryError::AccessDenied(_) => "AccessDenied",
    }
}

#[tokio::test]
async fn a_comment_codecommit_can_never_take_is_invalid_and_is_not_retried() {
    let same = "BeforeCommitIdAndAfterCommitIdAreSameException";
    let r = replay(vec![(
        400,
        J11,
        &json!({"__type": same, "message": "The before commit ID and the after commit ID are the same, which is not valid"}).to_string(),
    )]);
    let e = cc(&r)
        .post_comment(&a_comment(&"a".repeat(40), &"a".repeat(40)))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m.contains(same) && m.contains("are the same")),
        "{e}"
    );
    assert_eq!(e.error_type(), "Invalid");
}

#[tokio::test]
async fn input_validation_refusals_are_invalid_on_every_codecommit_call() {
    for kind in [
        "InvalidClientRequestTokenException",
        "InvalidCommitIdException",
        "InvalidPullRequestIdException",
        "InvalidRepositoryNameException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(
            kind,
            cc(&r)
                .post_comment(&a_comment(&"d".repeat(40), &"s".repeat(40)))
                .await,
        );
        assert_eq!(got, "Invalid", "post-comment {kind}");
    }
    for kind in [
        "InvalidTitleException",
        "InvalidDescriptionException",
        "InvalidClientRequestTokenException",
        "InvalidReferenceNameException",
        "InvalidTargetsException",
        "InvalidRepositoryNameException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(kind, cc(&r).create_pull_request(&a_pull_request()).await);
        assert_eq!(got, "Invalid", "create-pull-request {kind}");
    }
    let r = replay(vec![(
        400,
        J11,
        &service_error("InvalidPullRequestIdException"),
    )]);
    let got = kind_of(
        "InvalidPullRequestIdException",
        cc(&r).get_pull_request("7").await,
    );
    assert_eq!(got, "Invalid", "get-pull-request");
    for kind in [
        "InvalidApprovalRuleContentException",
        "InvalidApprovalRuleNameException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(
            kind,
            cc(&r)
                .create_approval_rule("7", "hamh-prcheck-required", "{}")
                .await,
        );
        assert_eq!(got, "Invalid", "create-approval-rule {kind}");
    }
    let r = replay(vec![(
        400,
        J11,
        &service_error("InvalidRepositoryNameException"),
    )]);
    let got = kind_of(
        "InvalidRepositoryNameException",
        cc(&r).get_repository(REPO).await,
    );
    assert_eq!(got, "Invalid", "get-repository");
    let r = replay(vec![(
        400,
        J11,
        &service_error("InvalidRepositoryDescriptionException"),
    )]);
    let got = kind_of(
        "InvalidRepositoryDescriptionException",
        cc(&r).create_repository(REPO, "d", &[]).await,
    );
    assert_eq!(got, "Invalid", "create-repository");
    let r = replay(vec![(
        400,
        J11,
        &service_error("InvalidBranchNameException"),
    )]);
    let got = kind_of(
        "InvalidBranchNameException",
        cc(&r).create_branch(REPO, "main", &"a".repeat(40)).await,
    );
    assert_eq!(got, "Invalid", "create-branch");
    let r = replay(vec![(
        400,
        J11,
        &service_error("InvalidBranchNameException"),
    )]);
    let got = kind_of(
        "InvalidBranchNameException",
        cc(&r).update_default_branch(REPO, "main").await,
    );
    assert_eq!(got, "Invalid", "update-default-branch");
}

#[tokio::test]
async fn throttling_server_faults_and_the_races_after_a_push_stay_retryable() {
    // A ...DoesNotExist answer right after the git push can be the push not yet visible to the API.
    for (status, kind) in [
        (400, "ThrottlingException"),
        (500, "InternalFailure"),
        (503, "ServiceUnavailableException"),
        (400, "CommitDoesNotExistException"),
        (400, "PullRequestDoesNotExistException"),
        (400, "RepositoryDoesNotExistException"),
        (400, "EncryptionKeyUnavailableException"),
    ] {
        let r = replay(vec![(status, J11, &service_error(kind))]);
        let got = kind_of(
            kind,
            cc(&r)
                .post_comment(&a_comment(&"d".repeat(40), &"s".repeat(40)))
                .await,
        );
        assert_eq!(got, "Unavailable", "post-comment {status} {kind}");
    }
    for kind in [
        "ReferenceDoesNotExistException",
        "ThrottlingException",
        "EncryptionKeyAccessDeniedException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(kind, cc(&r).create_pull_request(&a_pull_request()).await);
        assert_eq!(got, "Unavailable", "create-pull-request {kind}");
    }
    let r = replay(vec![(
        400,
        J11,
        &service_error("PullRequestDoesNotExistException"),
    )]);
    let got = kind_of(
        "PullRequestDoesNotExistException",
        cc(&r).get_pull_request("7").await,
    );
    assert_eq!(got, "Unavailable", "get-pull-request");
    let r = replay(vec![(
        400,
        J11,
        &service_error("CommitDoesNotExistException"),
    )]);
    let got = kind_of(
        "CommitDoesNotExistException",
        cc(&r).create_branch(REPO, "main", &"a".repeat(40)).await,
    );
    assert_eq!(got, "Unavailable", "create-branch");
}

// ---- the signed git remote ----------------------------------------------------------------

#[tokio::test]
async fn the_remote_is_the_https_endpoint_with_a_credential_signed_from_the_role_keys() {
    let creds = aws_credential_types::Credentials::new(
        "AKIDEXAMPLE",
        "test-secret-for-the-codecommit-git-vector",
        Some("test-session-token".into()),
        None,
        "test",
    );
    let remotes = CodeCommitRemotes {
        credentials: aws_credential_types::provider::SharedCredentialsProvider::new(creds),
        region: "us-west-2".into(),
        clock: || 1_790_337_600,
    };
    let remote = remotes.remote(REPO).await.unwrap();
    assert_eq!(
        remote.url,
        format!("https://git-codecommit.us-west-2.amazonaws.com/v1/repos/{REPO}")
    );
    let keys = AwsKeys {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "test-secret-for-the-codecommit-git-vector".into(),
        session_token: Some("test-session-token".into()),
    };
    assert_eq!(
        remote.credential,
        Some(git_credential(&keys, "us-west-2", REPO, 1_790_337_600))
    );
    assert_eq!(
        remote.credential.unwrap().password,
        "20260925T120000Z21a37a18a32361cd4287d1c2804cbc80faca3de60d1d7248eeb25f18a53ab0fe"
    );
}

// ---- S3 -----------------------------------------------------------------------------------

fn s3blobs(r: &StaticReplayClient) -> S3Blobs {
    S3Blobs {
        client: client!(aws_sdk_s3, r),
        bucket: "hamh-live-harness-800000000022".into(),
    }
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("delivery-s3-")
        .tempdir()
        .unwrap()
}

#[tokio::test]
async fn put_file_sends_the_sha256_as_metadata_and_checksum_with_the_bucket_default_encryption() {
    let t = scratch();
    let f = t.path().join("x.zip");
    std::fs::write(&f, b"zip bytes").unwrap();
    let sha = delivery::files::sha256_hex(b"zip bytes");
    let r = replay(vec![(200, "application/xml", "")]);
    s3blobs(&r)
        .put_file(
            &format!("runs/{RUN}/deliverable/x.zip"),
            &f,
            &PutMeta {
                content_type: "application/zip",
                sha256_hex: sha.clone(),
            },
        )
        .await
        .unwrap();
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.method(), "PUT");
    assert!(
        q.uri().contains(&format!("runs/{RUN}/deliverable/x.zip")),
        "{}",
        q.uri()
    );
    assert_eq!(q.headers().get("content-type"), Some("application/zip"));
    assert_eq!(q.headers().get("x-amz-meta-sha256"), Some(sha.as_str()));
    let digest: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&sha[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    assert_eq!(
        q.headers().get("x-amz-checksum-sha256"),
        Some(
            base64::engine::general_purpose::STANDARD
                .encode(digest)
                .as_str()
        )
    );
    assert_eq!(
        q.headers().get("x-amz-server-side-encryption"),
        None,
        "the bucket default applies"
    );
    assert_eq!(q.headers().get("content-length"), Some("9"));
}

#[tokio::test]
async fn get_to_file_writes_the_body_and_a_missing_key_is_none() {
    let t = scratch();
    let f = t.path().join("b.bundle");
    let r = replay(vec![
        (200, "application/octet-stream", "bundle bytes"),
        (
            404,
            "application/xml",
            "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>",
        ),
    ]);
    let b = s3blobs(&r);
    assert_eq!(
        b.get_to_file("runs/R/deliverable/verified.bundle", &f)
            .await
            .unwrap(),
        Some(12)
    );
    assert_eq!(std::fs::read(&f).unwrap(), b"bundle bytes");
    assert_eq!(
        b.get_to_file("runs/R/deliverable/verified.bundle", &f)
            .await
            .unwrap(),
        None
    );
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.method(), "GET");
    assert!(q.uri().contains("runs/R/deliverable/verified.bundle"));
}

/// S3 answers AccessDenied when the role has no grant for the key, and for a missing key when the role has no
/// `s3:ListBucket`. Every other S3 failure stays `Unavailable` and is retried.
#[tokio::test]
async fn an_s3_access_denied_is_its_own_error_type_and_other_s3_failures_stay_unavailable() {
    let denied = "<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>User: arn:aws:sts::700000000011:assumed-role/hamh-drill-bundle-validator/x is not authorized to perform: s3:GetObject on resource: arn:aws:s3:::hamh-drill-harness-700000000011/runs/R/input/repo.bundle because no identity-based policy allows the s3:GetObject action</Message><RequestId>R</RequestId></Error>";
    let internal = "<?xml version=\"1.0\"?><Error><Code>InternalError</Code><Message>We encountered an internal error. Please try again.</Message></Error>";
    let t = scratch();
    let f = t.path().join("b.bundle");
    let key = "runs/R/input/repo.bundle";

    let r = replay(vec![(403, "application/xml", denied)]);
    let e = s3blobs(&r).get_to_file(key, &f).await.unwrap_err();
    assert_eq!(e.error_type(), "AccessDenied", "get: {e}");
    assert!(
        e.to_string().contains(key) && e.to_string().contains("s3:GetObject"),
        "the message names the key and the denied action: {e}"
    );

    let body = t.path().join("x.zip");
    std::fs::write(&body, b"zip bytes").unwrap();
    let r = replay(vec![(403, "application/xml", denied)]);
    let e = s3blobs(&r)
        .put_file(
            "runs/R/deliverable/verified.bundle",
            &body,
            &PutMeta {
                content_type: "application/octet-stream",
                sha256_hex: delivery::files::sha256_hex(b"zip bytes"),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_type(), "AccessDenied", "put: {e}");

    let r = replay(vec![(403, "application/xml", denied)]);
    let e = s3blobs(&r).list("runs/R/evidence/").await.unwrap_err();
    assert_eq!(e.error_type(), "AccessDenied", "list: {e}");

    // A server fault and a missing key (with ListBucket, NoSuchKey) keep their meaning.
    let r = replay(vec![(500, "application/xml", internal)]);
    let e = s3blobs(&r).get_to_file(key, &f).await.unwrap_err();
    assert_eq!(e.error_type(), "Unavailable", "get 500: {e}");
    let r = replay(vec![(500, "application/xml", internal)]);
    let e = s3blobs(&r).list("runs/R/evidence/").await.unwrap_err();
    assert_eq!(e.error_type(), "Unavailable", "list 500: {e}");
}

#[tokio::test]
async fn list_follows_continuation_tokens() {
    let page = |keys: &[&str], next: Option<&str>| {
        let contents: String = keys
            .iter()
            .map(|k| format!("<Contents><Key>{k}</Key><Size>1</Size></Contents>"))
            .collect();
        let (trunc, token) = match next {
            Some(t) => (
                "true",
                format!("<NextContinuationToken>{t}</NextContinuationToken>"),
            ),
            None => ("false", String::new()),
        };
        format!(
            "<?xml version=\"1.0\"?><ListBucketResult><Name>b</Name><Prefix>runs/R/evidence/</Prefix><KeyCount>{}</KeyCount><IsTruncated>{trunc}</IsTruncated>{token}{contents}</ListBucketResult>",
            keys.len()
        )
    };
    let p1 = page(
        &["runs/R/evidence/a.json", "runs/R/evidence/b.json"],
        Some("tok"),
    );
    let p2 = page(&["runs/R/evidence/c.json"], None);
    let r = replay(vec![
        (200, "application/xml", &p1),
        (200, "application/xml", &p2),
    ]);
    let keys = s3blobs(&r).list("runs/R/evidence/").await.unwrap();
    assert_eq!(
        keys,
        vec![
            "runs/R/evidence/a.json",
            "runs/R/evidence/b.json",
            "runs/R/evidence/c.json"
        ]
    );
    let uris: Vec<String> = r.actual_requests().map(|q| q.uri().to_string()).collect();
    assert!(
        uris[0].contains("list-type=2") && uris[0].contains("prefix=runs%2FR%2Fevidence%2F"),
        "{}",
        uris[0]
    );
    assert!(uris[1].contains("continuation-token=tok"), "{}", uris[1]);
}

// ---- DynamoDB -----------------------------------------------------------------------------

fn table(r: &StaticReplayClient) -> DynamoTable {
    DynamoTable {
        client: client!(aws_sdk_dynamodb, r),
        table: "hamh-live-harness".into(),
    }
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

#[tokio::test]
async fn the_allowlist_read_is_a_consistent_get_of_the_target_key() {
    let item = json!({"Item": {
        "PK": {"S": format!("REPOALLOW#cc:{E2E_REGION}:{REPO}")}, "SK": {"S": "META"},
        "target": {"S": format!("cc:{REPO}")}, "addedBy": {"S": "arn:aws:sts::800000000022:assumed-role/Admin/testuser"},
        "addedAt": {"S": "2026-09-25T20:00:00Z"}
    }})
    .to_string();
    let r = replay(vec![(200, J10, &item), (200, J10, "{}")]);
    let t = table(&r);
    let target = format!("cc:{REPO}").parse().unwrap();
    let got = t.repo_allow(&target).await.unwrap().unwrap();
    assert_eq!(got.target, target);
    assert_eq!(t.repo_allow(&target).await.unwrap(), None);
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], "hamh-live-harness");
    assert_eq!(
        b["Key"]["PK"]["S"],
        format!("REPOALLOW#cc:{E2E_REGION}:{REPO}")
    );
    assert_eq!(b["Key"]["SK"]["S"], "META");
    assert_eq!(b["ConsistentRead"], true);
}

fn allow_new(name: &str) -> RepoAllowItem {
    RepoAllowItem {
        target: format!("cc:{name}").parse().unwrap(),
        added_by: format!("hamh-live-repo-gateway run {RUN}"),
        added_at: "2026-09-25T12:00:00Z".into(),
    }
}

#[tokio::test]
async fn the_harness_allowlist_entry_is_a_conditional_put_and_an_existing_item_is_returned() {
    const NEW: &str = "hamh-e2e-ntier-web-20261001";
    let stored = json!({"Item": {
        "PK": {"S": format!("REPOALLOW#cc:{E2E_REGION}:{NEW}")}, "SK": {"S": "META"},
        "target": {"S": format!("cc:{NEW}")}, "addedBy": {"S": "arn:aws:sts::800000000022:assumed-role/Admin/testuser"},
        "addedAt": {"S": "2026-09-25T20:00:00Z"}
    }})
    .to_string();
    // A new entry: the put, then the listing update; an existing one: the refused put, the read
    // and the listing update.
    let r = replay(vec![
        (200, J10, "{}"),
        (200, J10, "{}"),
        (400, J10, CCF),
        (200, J10, &stored),
        (200, J10, "{}"),
    ]);
    let t = table(&r);
    assert_eq!(t.add_repo_allow(&allow_new(NEW)).await.unwrap(), None);
    let existing = t.add_repo_allow(&allow_new(NEW)).await.unwrap().unwrap();
    assert_eq!(existing.target.to_string(), format!("cc:{NEW}"));
    assert_eq!(
        existing.added_by,
        "arn:aws:sts::800000000022:assumed-role/Admin/testuser"
    );
    let b = bodies(&r);
    assert_eq!(b[0]["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(
        b[0]["Item"]["PK"]["S"],
        format!("REPOALLOW#cc:{E2E_REGION}:{NEW}")
    );
    assert_eq!(b[0]["Item"]["SK"]["S"], "META");
    assert_eq!(b[0]["Item"]["target"]["S"], format!("cc:{NEW}"));
    assert_eq!(
        b[0]["Item"]["addedBy"]["S"],
        format!("hamh-live-repo-gateway run {RUN}")
    );
    // The listing `ham doctor` reads gains the target.
    assert_eq!(b[1]["Key"]["PK"]["S"], "REPOALLOW#INDEX");
    assert_eq!(b[1]["Key"]["SK"]["S"], "META");
    assert_eq!(b[1]["UpdateExpression"], "ADD targets :t");
    assert_eq!(
        b[1]["ExpressionAttributeValues"][":t"]["SS"],
        json!([format!("cc:{NEW}")])
    );
    assert_eq!(b[3]["ConsistentRead"], true);
    assert_eq!(b[4]["UpdateExpression"], "ADD targets :t");
    assert_eq!(b.len(), 5);
}

fn art() -> ArtifactItem {
    ArtifactItem {
        run_id: RUN.into(),
        artifact_id: "zip-0123456789abcdef0123".into(),
        kind: ArtifactKind::Zip,
        s3_uri: format!("s3://hamh-live-harness-800000000022/runs/{RUN}/deliverable/x.zip"),
        sha256: "ab".repeat(32),
        size_bytes: 1234,
    }
}

const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;

#[tokio::test]
async fn the_artifact_item_is_a_conditional_put_and_a_retry_of_the_same_item_succeeds() {
    let stored = json!({"Item": {
        "PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "ART#zip-0123456789abcdef0123"}, "runId": {"S": RUN},
        "artifactId": {"S": "zip-0123456789abcdef0123"}, "kind": {"S": "zip"},
        "s3Uri": {"S": format!("s3://hamh-live-harness-800000000022/runs/{RUN}/deliverable/x.zip")},
        "sha256": {"S": "ab".repeat(32)}, "sizeBytes": {"N": "1234"}
    }})
    .to_string();
    let r = replay(vec![(200, J10, "{}"), (400, J10, CCF), (200, J10, &stored)]);
    let t = table(&r);
    t.put_artifact(&art()).await.unwrap();
    t.put_artifact(&art()).await.unwrap();
    let b = &bodies(&r)[0];
    assert_eq!(b["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b["Item"]["PK"]["S"], format!("RUN#{RUN}"));
    assert_eq!(b["Item"]["SK"]["S"], "ART#zip-0123456789abcdef0123");
    assert_eq!(b["Item"]["kind"]["S"], "zip");
    assert_eq!(b["Item"]["sizeBytes"]["N"], "1234");
}

#[tokio::test]
async fn an_artifact_id_holding_another_item_is_an_error() {
    let other = json!({"Item": {
        "PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "ART#zip-0123456789abcdef0123"}, "runId": {"S": RUN},
        "artifactId": {"S": "zip-0123456789abcdef0123"}, "kind": {"S": "zip"}, "s3Uri": {"S": "s3://x/y"},
        "sha256": {"S": "cd".repeat(32)}, "sizeBytes": {"N": "1"}
    }})
    .to_string();
    let r = replay(vec![(400, J10, CCF), (200, J10, &other)]);
    let err = table(&r).put_artifact(&art()).await.unwrap_err();
    assert!(
        err.to_string().contains("ART#zip-0123456789abcdef0123"),
        "{err}"
    );
}

// ---- the pull request of a follow-up run --------------------------------------------------------------------

fn merged_pr_body(merge_metadata: Value) -> String {
    let mut target = json!({
        "repositoryName": REPO, "sourceReference": "refs/heads/harness/01K5ZQ4T8M3N6P7R9S0V1W2X4Z",
        "destinationReference": "refs/heads/main", "sourceCommit": "s".repeat(40), "destinationCommit": "d".repeat(40),
        "mergeBase": "b".repeat(40)
    });
    if !merge_metadata.is_null() {
        target["mergeMetadata"] = merge_metadata;
    }
    json!({"pullRequest": {
        "pullRequestId": "7", "title": "t", "revisionId": "rev-abc", "pullRequestStatus": "CLOSED",
        "pullRequestTargets": [target], "approvalRules": []
    }})
    .to_string()
}

#[tokio::test]
async fn get_pull_request_reads_the_repository_the_branches_the_merge_base_and_the_merge_state() {
    // PullRequestTarget (API reference): repositoryName, sourceReference, destinationReference, mergeBase, mergeMetadata.isMerged.
    let metadata = json!({"isMerged": true, "mergedBy": "arn:aws:iam::800000000022:user/x", "mergeOption": "SQUASH_MERGE", "mergeCommitId": "m".repeat(40)});
    let r = replay(vec![
        (200, J11, &merged_pr_body(metadata)),
        (200, J11, &merged_pr_body(json!({"isMerged": false}))),
        (200, J11, &merged_pr_body(Value::Null)),
    ]);
    let api = cc(&r);
    let merged = api.get_pull_request("7").await.unwrap();
    assert_eq!(merged.repository, REPO);
    assert_eq!(
        merged.source_ref,
        "refs/heads/harness/01K5ZQ4T8M3N6P7R9S0V1W2X4Z"
    );
    assert_eq!(merged.destination_ref, "refs/heads/main");
    assert_eq!(merged.merge_base, "b".repeat(40));
    assert!(merged.merged);
    assert!(
        !api.get_pull_request("7").await.unwrap().merged,
        "isMerged false"
    );
    assert!(
        !api.get_pull_request("7").await.unwrap().merged,
        "no merge metadata at all"
    );
}

#[tokio::test]
async fn create_pull_request_reads_the_same_fields_back() {
    let r = replay(vec![(200, J11, &pr_body(json!([])))]);
    let info = cc(&r).create_pull_request(&a_pull_request()).await.unwrap();
    assert_eq!(info.repository, REPO);
    assert_eq!(info.source_ref, format!("refs/heads/harness/{RUN}"));
    assert_eq!(info.destination_ref, "refs/heads/main");
    assert!(!info.merged);
}

#[tokio::test]
async fn answers_no_retry_can_change_are_invalid_on_a_follow_ups_calls() {
    // IdempotencyParameterMismatchException: the same token with a changed request (a follow-up whose old branch moved between two
    // attempts). SourceAndDestinationAreSameException and RepositoryNotAssociatedWithPullRequestException are as final.
    for kind in [
        "IdempotencyParameterMismatchException",
        "SourceAndDestinationAreSameException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(kind, cc(&r).create_pull_request(&a_pull_request()).await);
        assert_eq!(got, "Invalid", "create-pull-request {kind}");
    }
    for kind in [
        "IdempotencyParameterMismatchException",
        "RepositoryNotAssociatedWithPullRequestException",
    ] {
        let r = replay(vec![(400, J11, &service_error(kind))]);
        let got = kind_of(
            kind,
            cc(&r)
                .post_comment(&a_comment(&"d".repeat(40), &"s".repeat(40)))
                .await,
        );
        assert_eq!(got, "Invalid", "post-comment {kind}");
    }
}
