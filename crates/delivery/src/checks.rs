//! Target, ref and pull-request checks.
//! Every refusal carries the exact reason `ham` shows, and each one happens before
//! any repository read or push.

use crate::error::DeliveryError;
use crate::git::TreeEntry;
use crate::repo_names::RepoNames;
use harness_protocol::common::is_ulid;
use harness_protocol::store::RepoAllowItem;
use harness_protocol::target::{E2E_ACCOUNT, E2E_REGION, RepoTarget, new_repo_refusal};

/// The prefix of the default repository-name rule (`hamh-e2e-*`; [`RepoNames::default`]).
pub const REPO_PREFIX: &str = "hamh-e2e-";
/// The only refs `repo-gateway` pushes (`sourceRefPrefix`).
pub const HARNESS_REF_PREFIX: &str = "refs/heads/harness/";
/// Every pull request targets this (`destinationRef`).
pub const DESTINATION_REF: &str = "refs/heads/main";
pub const DESTINATION_BRANCH: &str = "main";
/// CodeCommit CreatePullRequest limits (`prTitleMaxChars`, `prDescriptionMaxChars`).
pub const PR_TITLE_MAX_CHARS: usize = 150;
pub const PR_DESCRIPTION_MAX_CHARS: usize = 10_240;
/// CodeCommit PostCommentForPullRequest `content` limit.
pub const COMMENT_MAX_CHARS: usize = 10_240;
/// The PR-check approval rule, attached to a pull request whose source commit has a [`BUILD_FILE`].
pub const PRCHECK_RULE: &str = "hamh-prcheck-required";
/// The file the PR check runs: the root `buildspec.yml` of the pull request's source commit (the `buildSpec` of the CodeBuild project in
/// `infra/lib/stacks/delivery.ts`). Without it the build fails and nobody approves, so a rule would stay unmet for ever.
pub const BUILD_FILE: &str = "buildspec.yml";
/// The comment (token `hamh-pr-<runId>-nocheck`) on a pull request that has no check rule because its source commit has no [`BUILD_FILE`].
/// It states what was not attached and claims nothing about the work, either way (root invariant 1).
pub const NO_CHECK_NOTE: &str = "No pull request check is attached. The source commit has no file named buildspec.yml at its root, so the harness did not add the approval rule hamh-prcheck-required. The harness does not block merging this pull request, and it does not say the changes passed a check. Review the changes yourself before you merge.";

/// Whether a root tree entry is the build file the PR check can run: a regular file (mode 100644 or 100755) named exactly
/// [`BUILD_FILE`]. A link, a folder, a submodule, a nested file and another spelling are not: CodeBuild would not read them as the
/// project's build file, and a link's target is not protected by the bundle validator (which protects only the path `buildspec.yml`),
/// so a run could write the check that approves its own pull request.
pub fn is_build_file(entry: &TreeEntry) -> bool {
    entry.path == BUILD_FILE
        && entry.kind == "blob"
        && matches!(entry.mode.as_str(), "100644" | "100755")
}

/// `refs/heads/harness/<runId>` for a ULID run id; the only value a push may target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessRef(String);

impl HarnessRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `harness/<runId>`, the branch name.
    pub fn branch(&self) -> &str {
        &self.0["refs/heads/".len()..]
    }
}

fn refused(msg: String) -> DeliveryError {
    DeliveryError::Refused(msg)
}

/// The CodeCommit repository name of a target the gateway serves under the default rule (`hamh-e2e-*`); every other target is refused.
pub fn codecommit_name(target: &RepoTarget) -> Result<&str, DeliveryError> {
    codecommit_name_in(target, &RepoNames::default())
}

/// The CodeCommit repository name of a target the gateway serves under the stage's repository-name rule; every other target is refused. A repository the harness
/// names itself (`hamh-e2e-new-<yyyymmdd>`, `hamh-e2e-ntier-<kind>-<yyyymmdd>`: the exact forms `new_repo_refusal` accepts, the names `--new-repo` creates) is covered
/// whatever the rule says, so a stage that lists only its own repositories can still make and use them. The default rule covers them anyway. A rule that lists the default
/// entry also covers a repository made under the earlier names ([`RepoNames::allows_earlier_name`]); `--new-repo` still makes only the new forms ([`check_new_repo_name`]).
pub fn codecommit_name_in<'a>(
    target: &'a RepoTarget,
    names: &RepoNames,
) -> Result<&'a str, DeliveryError> {
    match target {
        RepoTarget::CodeCommit { name }
            if names.allows(name)
                || names.allows_earlier_name(name) // COMPAT-CAH-DATA
                || new_repo_refusal(name).is_none() =>
        {
            Ok(name)
        }
        RepoTarget::CodeCommit { name } => Err(refused(names.refusal(name))),
        RepoTarget::GitHub { .. } => Err(refused(format!(
            "refused: {target} needs the tenant GitHub App (app mode), which is not installed"
        ))),
        RepoTarget::GitLab { .. } => Err(refused(format!(
            "refused: {target} is a GitLab target, which is not supported; repo-gateway makes no GitLab call"
        ))),
    }
}

/// The allowlist entry must exist under the target's key and name the same target. A missing entry is refused with the operator command that
/// adds it, not with the key of the table.
pub fn check_allowlisted(
    target: &RepoTarget,
    item: Option<&RepoAllowItem>,
) -> Result<(), DeliveryError> {
    let key = target.allowlist_key();
    match item {
        None => Err(refused(format!(
            "refused: {target} is not on the allowlist of this stage; an operator allows it with `ham-admin repos allow {target}`"
        ))),
        Some(i) if i.target != *target => Err(refused(format!(
            "refused: the allowlist entry {key} names {}",
            i.target
        ))),
        Some(_) => Ok(()),
    }
}

pub fn codecommit_arn(name: &str) -> String {
    format!("arn:aws:codecommit:{E2E_REGION}:{E2E_ACCOUNT}:{name}")
}

/// The repository CodeCommit describes must be `name` in the home account and Region
/// ([`E2E_ACCOUNT`], [`E2E_REGION`]).
pub fn check_repository_arn(
    name: &str,
    arn: Option<&str>,
    account_id: Option<&str>,
) -> Result<(), DeliveryError> {
    let Some(arn) = arn else {
        return Err(refused(format!(
            "refused: CodeCommit returned no ARN for cc:{name}"
        )));
    };
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let in_place = parts.len() == 6
        && parts[..3] == ["arn", "aws", "codecommit"]
        && parts[3] == E2E_REGION
        && parts[4] == E2E_ACCOUNT
        && account_id == Some(E2E_ACCOUNT);
    if !in_place {
        return Err(refused(format!(
            "refused: repository {arn} is outside {E2E_ACCOUNT} {E2E_REGION}"
        )));
    }
    if parts[5] != name {
        return Err(refused(format!(
            "refused: repository {arn} is not cc:{name}"
        )));
    }
    Ok(())
}

pub fn harness_ref(run_id: &str) -> Result<HarnessRef, DeliveryError> {
    if !is_ulid(run_id) {
        return Err(DeliveryError::invalid(format!(
            "runId {run_id:?} is not a ULID"
        )));
    }
    Ok(HarnessRef(format!("{HARNESS_REF_PREFIX}{run_id}")))
}

/// The ref a push-pr request may name: absent, `harness/<runId>` or `refs/heads/harness/<runId>`.
pub fn push_ref(run_id: &str, requested: Option<&str>) -> Result<HarnessRef, DeliveryError> {
    let ours = harness_ref(run_id)?;
    match requested {
        None => Ok(ours),
        Some(r) if r == ours.as_str() || r == ours.branch() => Ok(ours),
        Some(r) => Err(refused(format!(
            "refused: push to {r}; repo-gateway pushes only {}",
            ours.as_str()
        ))),
    }
}

/// `hamh-e2e-new-<yyyymmdd>` or `hamh-e2e-ntier-<kind>-<yyyymmdd>` with a calendar date
/// (`harness_protocol::target::new_repo_refusal`).
pub fn check_new_repo_name(name: &str) -> Result<(), DeliveryError> {
    match new_repo_refusal(name) {
        None => Ok(()),
        Some(reason) => Err(refused(reason)),
    }
}

/// A base branch as a full ref: `main` or `refs/heads/main` give `refs/heads/main`. Only branches
/// are served, and only names in `[A-Za-z0-9._/-]` that `git check-ref-format` accepts.
pub fn normalize_base_ref(base_ref: &str) -> Result<String, DeliveryError> {
    let bad = || DeliveryError::invalid(format!("baseRef {base_ref:?} is not a branch name"));
    let branch = match base_ref.strip_prefix("refs/") {
        Some(rest) => rest.strip_prefix("heads/").ok_or_else(bad)?,
        None => base_ref,
    };
    let chars_ok = branch
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-'));
    let parts_ok = branch.split('/').all(|p| {
        !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock") && !p.ends_with('.')
    });
    if branch.is_empty()
        || !chars_ok
        || !parts_ok
        || branch.contains("..")
        || branch.starts_with('-')
    {
        return Err(bad());
    }
    Ok(format!("refs/heads/{branch}"))
}

/// The key of an `s3://<bucket>/<key>` URI that names an object of this run in the harness bucket
/// (`runs/<runId>/...`, no empty, `.` or `..` segment).
pub fn run_object_key(uri: &str, bucket: &str, run_id: &str) -> Result<String, DeliveryError> {
    let bad = |why: String| DeliveryError::invalid(format!("{uri:?} {why}"));
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| bad("is not an s3:// URI".into()))?;
    let (b, key) = rest
        .split_once('/')
        .ok_or_else(|| bad("names no key".into()))?;
    if b != bucket {
        return Err(bad(format!("is outside the harness bucket {bucket}")));
    }
    let prefix = format!("runs/{run_id}/");
    let segments_ok = key
        .split('/')
        .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\\'));
    if !key.starts_with(&prefix) || !segments_ok {
        return Err(bad(format!("is not an object under {prefix}")));
    }
    Ok(key.to_string())
}

pub fn check_head_sha(sha: &str) -> Result<(), DeliveryError> {
    if sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(DeliveryError::invalid(format!(
            "{sha:?} is not a 40-character lowercase hex commit id"
        )))
    }
}

/// CodeCommit's limits count characters.
pub fn check_pr_text(title: &str, description: &str) -> Result<(), DeliveryError> {
    let t = title.chars().count();
    if title.trim().is_empty() {
        return Err(DeliveryError::invalid("the pull request title is blank"));
    }
    if t > PR_TITLE_MAX_CHARS {
        return Err(DeliveryError::invalid(format!(
            "the pull request title has {t} characters; CodeCommit allows {PR_TITLE_MAX_CHARS}"
        )));
    }
    let d = description.chars().count();
    if d > PR_DESCRIPTION_MAX_CHARS {
        return Err(DeliveryError::invalid(format!(
            "the pull request description has {d} characters; CodeCommit allows {PR_DESCRIPTION_MAX_CHARS} (post the tables as comments)"
        )));
    }
    Ok(())
}

pub fn check_comment(content: &str) -> Result<(), DeliveryError> {
    let n = content.chars().count();
    if n == 0 || n > COMMENT_MAX_CHARS {
        return Err(DeliveryError::invalid(format!(
            "a pull request comment has {n} characters; CodeCommit allows 1 to {COMMENT_MAX_CHARS}"
        )));
    }
    Ok(())
}

fn approvers_rule(pool_member: &str) -> String {
    serde_json::json!({
        "Version": "2018-11-08",
        "Statements": [{
            "Type": "Approvers",
            "NumberOfApprovalsNeeded": 1,
            "ApprovalPoolMembers": [pool_member],
        }],
    })
    .to_string()
}

/// `hamh-prcheck-required`: one approval, one pool member, the stage's PR-check build role.
pub fn prcheck_rule_content(stage: &str) -> String {
    approvers_rule(&format!(
        "arn:aws:sts::{E2E_ACCOUNT}:assumed-role/hamh-{stage}-prcheck-build/*"
    ))
}

pub fn pr_console_url(repository: &str, pull_request_id: &str) -> String {
    format!(
        "https://{E2E_REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/{repository}/pull-requests/{pull_request_id}/details"
    )
}

/// The pull request id in a console link that [`pr_console_url`] built for `repository`; `None` for any other text (a link of another
/// repository or Region, another page, an id that is not a number). A follow-up run reads the link its predecessor's `delivery.pr` event
/// kept, so the id is taken from it only when the link is exactly the one this crate writes.
pub fn pull_request_id_in(url: &str, repository: &str) -> Option<String> {
    let prefix = format!(
        "https://{E2E_REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/{repository}/pull-requests/"
    );
    let id = url
        .strip_prefix(prefix.as_str())?
        .strip_suffix("/details")?;
    // CodeCommit pull request ids are decimal numbers.
    let number = !id.is_empty() && id.len() <= 18 && id.bytes().all(|b| b.is_ascii_digit());
    number.then(|| id.to_string())
}
