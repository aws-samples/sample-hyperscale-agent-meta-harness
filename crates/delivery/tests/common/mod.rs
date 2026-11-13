//! Shared test fixtures: real temporary git repositories, a CodeCommit fake whose repositories
//! are local bare repositories (so `git fetch` and `git push` run for real), an in-memory S3, an
//! allowlist and a local git remote factory.
#![allow(dead_code, clippy::unwrap_used)]

use delivery::checks::codecommit_arn;
use delivery::error::DeliveryError;
use delivery::gateway::{
    CodeCommit, Created, NewComment, NewPullRequest, PullRequestInfo, Remotes, RepoMeta,
};
use delivery::git::Remote;
use delivery::ports::{Allowlist, Artifacts, Blobs, PutMeta};
use harness_protocol::store::{ArtifactItem, RepoAllowItem};
use harness_protocol::target::RepoTarget;
/// The home account and Region of the `accounts.json` entry the build selected.
pub use harness_protocol::target::{E2E_ACCOUNT as ACCOUNT, E2E_REGION as REGION};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
pub const BUCKET: &str = "hamh-live-harness-800000000022";
pub const FIXTURE: &str = "hamh-e2e-python-calc-20260925";
/// PostCommentForPullRequest's answer when `beforeCommitId` equals `afterCommitId` (the live text), as
/// the adapter (`aws.rs`) words it.
pub const EQUAL_COMMITS_REFUSAL: &str = "CodeCommit PostCommentForPullRequest: BeforeCommitIdAndAfterCommitIdAreSameException: The before commit ID and the after commit ID are the same, which is not valid";

/// A pinned inference Region other than the home Region.
pub fn other_region() -> &'static str {
    if REGION == "us-east-1" {
        "us-west-2"
    } else {
        "us-east-1"
    }
}

/// A 12-digit account number other than the stage account, whichever `accounts.json` entry the build selected (`ACCOUNT` is the account the build embeds).
pub fn other_account() -> &'static str {
    if ACCOUNT == "123456789012" {
        "210987654321"
    } else {
        "123456789012"
    }
}

/// A scratch directory under `TMPDIR`.
pub fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("delivery-test-")
        .tempdir()
        .unwrap()
}

/// Runs git with a fixed identity and no global or system config; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1790337600 +0000")
        .env("GIT_COMMITTER_DATE", "@1790337600 +0000")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

/// A working repository on `main` with one commit per file set; returns (path, shas).
pub fn work_repo(root: &Path, name: &str, commits: &[&[(&str, &str)]]) -> (PathBuf, Vec<String>) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let mut shas = Vec::new();
    for (i, files) in commits.iter().enumerate() {
        write_files(&dir, files);
        git(&dir, &["add", "-A"]);
        git(
            &dir,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("commit {i}"),
            ],
        );
        shas.push(git(&dir, &["rev-parse", "HEAD"]));
    }
    (dir, shas)
}

pub fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (path, body) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

/// Commits `files` (a `None` body deletes the path) on the current branch; returns the new sha.
pub fn commit(dir: &Path, files: &[(&str, Option<&str>)], msg: &str) -> String {
    for (path, body) in files {
        let p = dir.join(path);
        match body {
            Some(b) => {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b).unwrap();
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Publishes `work` as the bare repository `<remotes>/<name>.git` (what CodeCommit would hold).
pub fn publish(work: &Path, remotes: &Path, name: &str) -> PathBuf {
    let bare = remotes.join(format!("{name}.git"));
    git(
        remotes,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

/// A full bundle of `harness/<RUN>` at the work repo's HEAD (what FinalGates stages).
pub fn verified_bundle(work: &Path, run: &str, out: &Path) -> String {
    let branch = format!("harness/{run}");
    git(work, &["branch", "-f", &branch, "HEAD"]);
    git(
        work,
        &["bundle", "create", "-q", out.to_str().unwrap(), &branch],
    );
    git(work, &["rev-parse", "HEAD"])
}

pub fn refs(bare: &Path) -> BTreeMap<String, String> {
    git(bare, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(r, s)| (r.to_string(), s.to_string()))
        .collect()
}

// ---- fakes --------------------------------------------------------------------------------

type Objects = BTreeMap<String, (Vec<u8>, Option<PutMeta>)>;
/// (name, description, tags) of each CreateRepository.
type CreatedRepo = (String, String, Vec<(String, String)>);

#[derive(Default)]
pub struct MemBlobs {
    pub objects: Mutex<Objects>,
    pub reads: Mutex<Vec<String>>,
    /// Key prefixes the role has no grant for: a read of a key under one, and a listing of a prefix under one, answer
    /// `AccessDenied` as S3 does (the message names the operation and the key).
    pub denied: Mutex<Vec<String>>,
    /// Older versions of a key, by (key, version id); the current object stays in `objects`.
    pub versions: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl MemBlobs {
    /// Stores `body` as the version `version_id` of `key` and makes it the current object, as a PUT to a versioned bucket does.
    pub fn insert_version(&self, key: &str, version_id: &str, body: Vec<u8>) {
        self.versions
            .lock()
            .unwrap()
            .insert((key.into(), version_id.into()), body.clone());
        self.insert(key, body);
    }
    /// Takes away the grant for everything under `prefix`.
    pub fn deny(&self, prefix: &str) {
        self.denied.lock().unwrap().push(prefix.into());
    }
    fn is_denied(&self, key: &str) -> bool {
        self.denied
            .lock()
            .unwrap()
            .iter()
            .any(|d| key.starts_with(d.as_str()))
    }
    pub fn insert(&self, key: &str, body: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), (body, None));
    }
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(b, _)| b.clone())
    }
    pub fn meta(&self, key: &str) -> Option<PutMeta> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .and_then(|(_, m)| m.clone())
    }
    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }
}

impl Blobs for MemBlobs {
    async fn get_to_file(&self, key: &str, path: &Path) -> Result<Option<u64>, DeliveryError> {
        self.reads.lock().unwrap().push(key.into());
        if self.is_denied(key) {
            return Err(DeliveryError::AccessDenied(format!(
                "s3 get {key}: AccessDenied"
            )));
        }
        match self.get(key) {
            Some(b) => {
                std::fs::write(path, &b).unwrap();
                Ok(Some(b.len() as u64))
            }
            None => Ok(None),
        }
    }
    async fn get_version_to_file(
        &self,
        key: &str,
        version_id: &str,
        path: &Path,
    ) -> Result<Option<u64>, DeliveryError> {
        self.reads
            .lock()
            .unwrap()
            .push(format!("{key}@{version_id}"));
        if self.is_denied(key) {
            return Err(DeliveryError::AccessDenied(format!(
                "s3 get {key}: AccessDenied"
            )));
        }
        let found = self
            .versions
            .lock()
            .unwrap()
            .get(&(key.to_string(), version_id.to_string()))
            .cloned();
        match found {
            Some(b) => {
                std::fs::write(path, &b).unwrap();
                Ok(Some(b.len() as u64))
            }
            None => Ok(None),
        }
    }
    async fn put_file(&self, key: &str, path: &Path, meta: &PutMeta) -> Result<(), DeliveryError> {
        let body = std::fs::read(path).unwrap();
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), (body, Some(meta.clone())));
        Ok(())
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, DeliveryError> {
        if self.is_denied(prefix) {
            return Err(DeliveryError::AccessDenied(format!(
                "s3 list {prefix}: AccessDenied"
            )));
        }
        Ok(self
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(prefix))
            .collect())
    }
}

#[derive(Default)]
pub struct FakeAllowlist {
    pub entries: Mutex<BTreeSet<String>>,
    /// An item whose body names another target, stored under this key.
    pub mismatched: Mutex<BTreeMap<String, String>>,
    pub reads: Mutex<u32>,
    /// Items `add_repo_allow` wrote (the harness's own entries for `--new-repo`).
    pub written: Mutex<Vec<RepoAllowItem>>,
}

impl FakeAllowlist {
    pub fn with(targets: &[&str]) -> Self {
        let a = Self::default();
        for t in targets {
            a.entries.lock().unwrap().insert((*t).into());
        }
        a
    }
}

fn allow_item(t: &str) -> RepoAllowItem {
    RepoAllowItem {
        target: t.parse().unwrap(),
        added_by: format!("arn:aws:sts::{ACCOUNT}:assumed-role/Admin/testuser"),
        added_at: "2026-09-25T20:00:00Z".into(),
    }
}

impl Allowlist for FakeAllowlist {
    async fn repo_allow(
        &self,
        target: &RepoTarget,
    ) -> Result<Option<RepoAllowItem>, DeliveryError> {
        *self.reads.lock().unwrap() += 1;
        let t = target.to_string();
        if let Some(other) = self.mismatched.lock().unwrap().get(&t) {
            return Ok(Some(allow_item(other)));
        }
        Ok(self
            .entries
            .lock()
            .unwrap()
            .contains(&t)
            .then(|| allow_item(&t)))
    }

    async fn add_repo_allow(
        &self,
        item: &RepoAllowItem,
    ) -> Result<Option<RepoAllowItem>, DeliveryError> {
        let t = item.target.to_string();
        if let Some(other) = self.mismatched.lock().unwrap().get(&t) {
            return Ok(Some(allow_item(other)));
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.contains(&t) {
            return Ok(Some(allow_item(&t)));
        }
        entries.insert(t);
        self.written.lock().unwrap().push(item.clone());
        Ok(None)
    }
}

#[derive(Default)]
pub struct FakeArtifacts {
    pub items: Mutex<Vec<ArtifactItem>>,
}

impl Artifacts for FakeArtifacts {
    async fn put_artifact(&self, item: &ArtifactItem) -> Result<(), DeliveryError> {
        let mut items = self.items.lock().unwrap();
        match items.iter().find(|i| i.artifact_id == item.artifact_id) {
            Some(i) if i == item => Ok(()),
            Some(_) => Err(DeliveryError::unavailable(
                "artifact exists with other content",
            )),
            None => {
                items.push(item.clone());
                Ok(())
            }
        }
    }
}

/// Remote URLs are the fake's bare repositories; no credential.
pub struct LocalRemotes {
    pub root: PathBuf,
    pub calls: Mutex<Vec<String>>,
}

impl LocalRemotes {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl Remotes for LocalRemotes {
    async fn remote(&self, repository: &str) -> Result<Remote, DeliveryError> {
        self.calls.lock().unwrap().push(repository.into());
        Ok(Remote {
            url: self
                .root
                .join(format!("{repository}.git"))
                .display()
                .to_string(),
            credential: None,
        })
    }
}

/// CodeCommit with repositories as bare repositories under `root`.
pub struct FakeCodeCommit {
    pub root: PathBuf,
    /// name -> (arn, account id) returned by GetRepository instead of the real place.
    pub place: Mutex<BTreeMap<String, (String, String)>>,
    pub calls: Mutex<Vec<String>>,
    pub created: Mutex<Vec<CreatedRepo>>,
    pub prs: Mutex<Vec<(NewPullRequest, PullRequestInfo)>>,
    pub rules: Mutex<BTreeMap<(String, String), String>>,
    pub comments: Mutex<Vec<NewComment>>,
    pub default_branch: Mutex<BTreeMap<String, String>>,
    /// Repository descriptions, set by CreateRepository or [`FakeCodeCommit::seed`].
    pub descriptions: Mutex<BTreeMap<String, String>>,
}

impl FakeCodeCommit {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            place: Mutex::default(),
            calls: Mutex::default(),
            created: Mutex::default(),
            prs: Mutex::default(),
            rules: Mutex::default(),
            comments: Mutex::default(),
            default_branch: Mutex::default(),
            descriptions: Mutex::default(),
        }
    }
    /// A repository someone else created: an empty bare repository with this description.
    pub fn seed(&self, name: &str, description: &str) -> PathBuf {
        git(
            &self.root,
            &["init", "-q", "--bare", &format!("{name}.git")],
        );
        self.descriptions
            .lock()
            .unwrap()
            .insert(name.into(), description.into());
        self.bare(name)
    }
    fn bare(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.git"))
    }
    fn call(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn rev(&self, name: &str, r: &str) -> Option<String> {
        let out = Command::new("git")
            .current_dir(self.bare(name))
            .args(["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl CodeCommit for FakeCodeCommit {
    async fn get_repository(&self, name: &str) -> Result<Option<RepoMeta>, DeliveryError> {
        self.call(format!("GetRepository {name}"));
        if !self.bare(name).exists() {
            return Ok(None);
        }
        let (arn, account) = self
            .place
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_else(|| (codecommit_arn(name), ACCOUNT.into()));
        let default_branch = self
            .default_branch
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .or_else(|| self.rev(name, "refs/heads/main").map(|_| "main".into()));
        let description = self.descriptions.lock().unwrap().get(name).cloned();
        Ok(Some(RepoMeta {
            name: name.into(),
            arn: Some(arn),
            account_id: Some(account),
            default_branch,
            description,
        }))
    }

    async fn create_repository(
        &self,
        name: &str,
        description: &str,
        tags: &[(String, String)],
    ) -> Result<Created, DeliveryError> {
        self.call(format!("CreateRepository {name}"));
        if self.bare(name).exists() {
            return Ok(Created::Existed);
        }
        git(
            &self.root,
            &["init", "-q", "--bare", &format!("{name}.git")],
        );
        self.created
            .lock()
            .unwrap()
            .push((name.into(), description.into(), tags.to_vec()));
        self.descriptions
            .lock()
            .unwrap()
            .insert(name.into(), description.into());
        Ok(Created::Created)
    }

    async fn create_branch(
        &self,
        repository: &str,
        branch: &str,
        commit_id: &str,
    ) -> Result<Created, DeliveryError> {
        self.call(format!("CreateBranch {repository} {branch} {commit_id}"));
        if self
            .rev(repository, &format!("refs/heads/{branch}"))
            .is_some()
        {
            return Ok(Created::Existed);
        }
        git(
            &self.bare(repository),
            &["update-ref", &format!("refs/heads/{branch}"), commit_id],
        );
        Ok(Created::Created)
    }

    async fn update_default_branch(
        &self,
        repository: &str,
        branch: &str,
    ) -> Result<(), DeliveryError> {
        self.call(format!("UpdateDefaultBranch {repository} {branch}"));
        self.default_branch
            .lock()
            .unwrap()
            .insert(repository.into(), branch.into());
        Ok(())
    }

    async fn create_pull_request(
        &self,
        pr: &NewPullRequest,
    ) -> Result<PullRequestInfo, DeliveryError> {
        self.call(format!("CreatePullRequest {}", pr.repository));
        let mut prs = self.prs.lock().unwrap();
        // CreatePullRequest API reference: the token "ensures the request cannot be repeated with a changed parameter"
        // (IdempotencyParameterMismatchException, a 400 no retry can change); the same parameters return the first request's pull request.
        if let Some((first, info)) = prs
            .iter()
            .find(|(p, _)| p.client_request_token == pr.client_request_token)
        {
            if first != pr {
                return Err(DeliveryError::invalid(
                    "CodeCommit CreatePullRequest: IdempotencyParameterMismatchException: the client request token is not valid. Either the token is not in a valid format, or the token has been used in a previous request and cannot be reused",
                ));
            }
            return Ok(info.clone());
        }
        // SourceAndDestinationAreSameException: "You must specify different branches for the source and destination."
        if pr.source_ref == pr.destination_ref {
            return Err(DeliveryError::invalid(
                "CodeCommit CreatePullRequest: SourceAndDestinationAreSameException: the source branch and destination branch for the pull request are the same",
            ));
        }
        // InvalidReferenceNameException: "Reference names must conform to the Git references format (for example, refs/heads/main)".
        for r in [&pr.source_ref, &pr.destination_ref] {
            if !r.starts_with("refs/heads/") {
                return Err(DeliveryError::invalid(format!(
                    "CodeCommit CreatePullRequest: InvalidReferenceNameException: {r}"
                )));
            }
        }
        // ReferenceDoesNotExistException: the adapter retries it (right after a push it can be a lag), so it is `Unavailable`.
        let source = self
            .rev(&pr.repository, &pr.source_ref)
            .ok_or_else(|| DeliveryError::unavailable("ReferenceDoesNotExistException: source"))?;
        let dest = self
            .rev(&pr.repository, &pr.destination_ref)
            .ok_or_else(|| {
                DeliveryError::unavailable("ReferenceDoesNotExistException: destination")
            })?;
        let merge_base = git(&self.bare(&pr.repository), &["merge-base", &source, &dest]);
        let id = (prs.len() + 1).to_string();
        let info = PullRequestInfo {
            pull_request_id: id.clone(),
            revision_id: format!("rev-{id}"),
            source_commit: source,
            destination_commit: dest,
            repository: pr.repository.clone(),
            source_ref: pr.source_ref.clone(),
            destination_ref: pr.destination_ref.clone(),
            merge_base,
            merged: false,
            approval_rules: Vec::new(),
        };
        prs.push((pr.clone(), info.clone()));
        Ok(info)
    }

    async fn get_pull_request(
        &self,
        pull_request_id: &str,
    ) -> Result<PullRequestInfo, DeliveryError> {
        self.call(format!("GetPullRequest {pull_request_id}"));
        let prs = self.prs.lock().unwrap();
        let (_, info) = prs
            .iter()
            .find(|(_, i)| i.pull_request_id == pull_request_id)
            .ok_or_else(|| DeliveryError::unavailable("PullRequestDoesNotExistException"))?;
        let mut info = info.clone();
        info.approval_rules = self
            .rules
            .lock()
            .unwrap()
            .iter()
            .filter(|((id, _), _)| id == pull_request_id)
            .map(|((_, n), c)| (n.clone(), c.clone()))
            .collect();
        Ok(info)
    }

    async fn create_approval_rule(
        &self,
        pull_request_id: &str,
        name: &str,
        content: &str,
    ) -> Result<Created, DeliveryError> {
        self.call(format!(
            "CreatePullRequestApprovalRule {pull_request_id} {name}"
        ));
        let mut rules = self.rules.lock().unwrap();
        let key = (pull_request_id.to_string(), name.to_string());
        if rules.contains_key(&key) {
            return Ok(Created::Existed);
        }
        rules.insert(key, content.into());
        Ok(Created::Created)
    }

    async fn post_comment(&self, c: &NewComment) -> Result<(), DeliveryError> {
        self.call(format!("PostCommentForPullRequest {}", c.pull_request_id));
        // CodeCommit refuses a comment on a pull request with no commits,
        // and the adapter answers it as `Invalid`.
        if c.before_commit_id == c.after_commit_id {
            return Err(DeliveryError::invalid(EQUAL_COMMITS_REFUSAL));
        }
        // PostCommentForPullRequest API reference: PullRequestDoesNotExistException (the adapter retries it: `Unavailable`),
        // RepositoryNotAssociatedWithPullRequestException ("The repository does not contain any pull requests with that pull request ID"),
        // CommitDoesNotExistException (`Unavailable` like the other races after a push) and IdempotencyParameterMismatchException.
        let prs = self.prs.lock().unwrap();
        let Some((_, info)) = prs
            .iter()
            .find(|(_, i)| i.pull_request_id == c.pull_request_id)
        else {
            return Err(DeliveryError::unavailable(
                "PullRequestDoesNotExistException",
            ));
        };
        if info.repository != c.repository {
            return Err(DeliveryError::invalid(
                "CodeCommit PostCommentForPullRequest: RepositoryNotAssociatedWithPullRequestException: the repository does not contain any pull requests with that pull request ID",
            ));
        }
        for id in [&c.before_commit_id, &c.after_commit_id] {
            if self.rev(&c.repository, id).is_none() {
                return Err(DeliveryError::unavailable("CommitDoesNotExistException"));
            }
        }
        let mut comments = self.comments.lock().unwrap();
        if let Some(first) = comments
            .iter()
            .find(|x| x.client_request_token == c.client_request_token)
        {
            if first != c {
                return Err(DeliveryError::invalid(
                    "CodeCommit PostCommentForPullRequest: IdempotencyParameterMismatchException: the client request token has been used in a previous request and cannot be reused",
                ));
            }
            return Ok(());
        }
        comments.push(c.clone());
        Ok(())
    }
}

impl FakeCodeCommit {
    /// The user merges pull request `id` in the console. `squash` puts one new commit on the destination branch that holds the source's
    /// tree (the source commit itself is not in the destination's history); otherwise the destination branch moves to the source commit
    /// (fast-forward). Either way the pull request reports `mergeMetadata.isMerged`.
    pub fn merge_pull_request(&self, id: &str, squash: bool) {
        let mut prs = self.prs.lock().unwrap();
        let (new, info) = prs
            .iter_mut()
            .find(|(_, i)| i.pull_request_id == id)
            .unwrap();
        let bare = self.bare(&new.repository);
        let dest = new.destination_ref.clone();
        if squash {
            let tree = git(
                &bare,
                &["rev-parse", &format!("{}^{{tree}}", info.source_commit)],
            );
            let squashed = git(
                &bare,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &info.destination_commit,
                    "-m",
                    "squash",
                ],
            );
            git(&bare, &["update-ref", &dest, &squashed]);
        } else {
            git(&bare, &["update-ref", &dest, &info.source_commit]);
        }
        info.merged = true;
    }

    /// Someone pushed `commit` to `branch` of `repository` (the bare repository takes any ref; the role's own limits are IAM's).
    pub fn set_branch(&self, repository: &str, branch: &str, commit: &str) {
        git(
            &self.bare(repository),
            &["update-ref", &format!("refs/heads/{branch}"), commit],
        );
    }

    /// The user deleted `branch`.
    pub fn delete_branch(&self, repository: &str, branch: &str) {
        git(
            &self.bare(repository),
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
    }

    /// Edits what `GetPullRequest` reports for pull request `id`.
    pub fn edit_pull_request(&self, id: &str, edit: impl FnOnce(&mut PullRequestInfo)) {
        let mut prs = self.prs.lock().unwrap();
        let (_, info) = prs
            .iter_mut()
            .find(|(_, i)| i.pull_request_id == id)
            .unwrap();
        edit(info);
    }
}
