//! `bundle-validator`.
//! It loads the verified bundle and returns pass or fail
//! with every violation:
//!
//! 0. With `sourceBundleUri` (a worker turn bundle in the harness bucket, full history, holding
//!    `headSha`), the validator first writes the verified bundle itself: a full-history bundle of
//!    `refs/heads/harness/<runId>` at `headSha` at `runs/<runId>/deliverable/verified.bundle`, with
//!    its SHA-256 metadata. A source that is missing, does not verify or lacks the head is a
//!    `bundle` violation and nothing is written.
//! 1. `bundle`, `branch`: the bundle verifies and holds `refs/heads/harness/<runId>` at `headSha`.
//! 2. `history`: `baseSha` is in the bundle and an ancestor of the head (absent base: empty tree).
//! 3. Per changed path of `base..head` (renames as delete plus add), in git's path order:
//!    a protected path fails (`workflow-file`, `buildspec`, `engine-config`); else the path must
//!    match the allowed globs, the last matching glob deciding and `!` negating (`path-glob`).
//! 4. `bundle-size`, `file-size`: the bundle and each added or modified blob within the limits.
//! 5. `secret`: gitleaks over the commits of `base..head` with the default rules, `gitleaks:allow`
//!    comments ignored, and no repository-supplied config; findings carry rule, file, line and
//!    commit, never the secret.
//! 6. `third-party-copy`: a file the run delivers, or a blob anywhere in the history `base..head`, that
//!    equals a file the run stored through the research route (see [`crate::third_party`]). With the request's `thirdParty` set to
//!    `allow` the same findings are reported under `findings` and do not fail the validation.
//!
//! `engineConfig`: `all`, the default, protects every engine-configuration path; a direct run sends
//! `executable-only`, which protects only what an engine would execute (hooks, plugins, agents), because Cline writes
//! `.clinerules/*.md` and `.cline/rules/*.md` as ordinary project notes and a finished run must not be discarded for one.

use crate::bundle::load_verified;
use crate::checks::{check_head_sha, harness_ref, run_object_key};
use crate::error::DeliveryError;
use crate::files::{scratch, sha256_file};
use crate::git::{Change, EMPTY_TREE, Git, Remote, TreeEntry};
use crate::glob::glob_match;
use crate::ports::{Blobs, PutMeta};
use crate::third_party;
use harness_protocol::store::s3::verified_bundle_key;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Size limits. A file: 100 MiB, GitHub's per-file push limit and the tighter of the two git hosts (CodeCommit takes 2 GB over
/// git); GitHub is out of scope for now and the limit stays as states it. A bundle: 2 GiB, so the
/// bundle, its unpacked copy and the zip fit the 10,240 MB Lambda ephemeral storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_bundle_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 100 * 1024 * 1024,
            max_bundle_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateRequest {
    pub run_id: String,
    /// `None` for an empty base.
    #[serde(default)]
    pub base_sha: Option<String>,
    pub head_sha: String,
    /// The union of the run's task write globs.
    pub allowed_globs: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
    /// A worker turn bundle to build the verified bundle from (`s3://<harness bucket>/runs/<runId>/...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bundle_uri: Option<String>,
    /// Which engine-configuration paths are protected; the default (`all`) is left off the wire.
    #[serde(default, skip_serializing_if = "EngineConfigMode::is_all")]
    pub engine_config: EngineConfigMode,
    /// What a delivered copy of a stored third-party file does; the default (`block`) is left off the wire.
    #[serde(default, skip_serializing_if = "ThirdPartyMode::is_block")]
    pub third_party: ThirdPartyMode,
}

/// What the validator does with a delivered file that equals a file the run stored through the research route: the one type of
/// `harness_protocol::research` (the start request's `thirdParty` member carries the same words).
pub use harness_protocol::research::ThirdPartyMode;

/// How much of the engine-configuration tree a change may not touch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineConfigMode {
    /// All of `.cline/**`, `.clinerules/**`, `opencode.json` and `.opencode/**`.
    #[default]
    All,
    /// Only what an engine would execute: `.cline/hooks/`, `.cline/plugins/`, `.cline/agents/`, `.clinerules/hooks/`,
    /// `opencode.json` and `.opencode/`. Rule files such as `.clinerules/*.md` are ordinary project files.
    ExecutableOnly,
}

impl EngineConfigMode {
    pub fn is_all(&self) -> bool {
        *self == EngineConfigMode::All
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    Bundle,
    Branch,
    History,
    WorkflowFile,
    Buildspec,
    EngineConfig,
    PathGlob,
    BundleSize,
    FileSize,
    Secret,
    ThirdPartyCopy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub rule: Rule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateOutput {
    pub pass: bool,
    pub run_id: String,
    pub base_sha: Option<String>,
    pub head_sha: String,
    pub changed_files: u64,
    pub violations: Vec<Violation>,
    /// What the validator found and let through because the request allowed it (`thirdParty: allow`); empty and off the wire
    /// otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Violation>,
    /// The verified bundle that was validated (the one `push-pr` and `packager` read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_bundle_uri: Option<String>,
}

/// Paths no agent may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protected {
    /// `.github/workflows/**`: GitHub Actions on the production path.
    Workflow,
    /// The root `buildspec.yml` the PR check runs.
    Buildspec,
    /// `.cline/**`, `.clinerules/**`, `opencode.json`, `.opencode/**`.
    EngineConfig,
}

pub fn protected(path: &str) -> Option<Protected> {
    protected_in(EngineConfigMode::All, path)
}

/// [`protected`] for a request's `engineConfig` mode. Workflow files and the PR-check buildspec are protected in both.
pub fn protected_in(mode: EngineConfigMode, path: &str) -> Option<Protected> {
    if path.starts_with(".github/workflows/") {
        Some(Protected::Workflow)
    } else if path == "buildspec.yml" {
        Some(Protected::Buildspec)
    } else if engine_config_path(mode, path) {
        Some(Protected::EngineConfig)
    } else {
        None
    }
}

fn engine_config_path(mode: EngineConfigMode, path: &str) -> bool {
    match mode {
        EngineConfigMode::All => {
            path.starts_with(".cline/")
                || path.starts_with(".clinerules/")
                || path == "opencode.json"
                || path.starts_with(".opencode/")
        }
        EngineConfigMode::ExecutableOnly => {
            path.starts_with(".cline/hooks/")
                || path.starts_with(".cline/plugins/")
                || path.starts_with(".cline/agents/")
                || path.starts_with(".clinerules/hooks/")
                || path == "opencode.json"
                || path.starts_with(".opencode/")
        }
    }
}

/// The last glob that matches decides; `!glob` excludes; no glob allows nothing.
pub fn path_allowed(globs: &[String], path: &str) -> bool {
    let mut allowed = false;
    for g in globs {
        match g.strip_prefix('!') {
            Some(neg) if glob_match(neg, path) => allowed = false,
            Some(_) => {}
            None if glob_match(g, path) => allowed = true,
            None => {}
        }
    }
    allowed
}

fn violation(rule: Rule, path: Option<&str>, detail: String) -> Violation {
    Violation {
        rule,
        path: path.map(str::to_string),
        detail,
    }
}

/// The per-path rules (3 and the file half of 4) over a diff and the head tree.
pub fn evaluate(
    changes: &[Change],
    tree: &BTreeMap<String, TreeEntry>,
    globs: &[String],
    limits: &Limits,
) -> Vec<Violation> {
    evaluate_in(EngineConfigMode::All, changes, tree, globs, limits)
}

/// [`evaluate`] with the request's `engineConfig` mode.
pub fn evaluate_in(
    mode: EngineConfigMode,
    changes: &[Change],
    tree: &BTreeMap<String, TreeEntry>,
    globs: &[String],
    limits: &Limits,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for c in changes {
        let p = c.path.as_str();
        let at_head = tree.get(p);
        if let Some(kind) = protected_in(mode, p) {
            let (rule, what) = match kind {
                Protected::Workflow => (Rule::WorkflowFile, "a workflow file"),
                Protected::Buildspec => (Rule::Buildspec, "the PR-check buildspec"),
                Protected::EngineConfig => (Rule::EngineConfig, "engine configuration"),
            };
            out.push(violation(
                rule,
                Some(p),
                format!("{p} is {what}; the change ({}) is refused", c.status),
            ));
        } else if !path_allowed(globs, p) {
            out.push(violation(
                Rule::PathGlob,
                Some(p),
                format!("{p} ({}) is outside the allowed globs", c.status),
            ));
        }
        if c.status != 'D'
            && let Some(size) = at_head.and_then(|e| e.size)
            && size > limits.max_file_bytes
        {
            out.push(violation(
                Rule::FileSize,
                Some(p),
                format!(
                    "{p} is {size} bytes; the limit is {} bytes",
                    limits.max_file_bytes
                ),
            ));
        }
    }
    out
}

/// One gitleaks finding, without the secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub rule_id: String,
    pub file: String,
    pub line: u64,
    pub commit: String,
}

pub trait SecretScanner: Send + Sync {
    /// Scans the commits `range` selects (`git log` syntax) in a bare repository.
    fn scan(
        &self,
        repo: &Path,
        range: &str,
    ) -> impl Future<Output = Result<Vec<Finding>, DeliveryError>> + Send;
}

/// The pinned gitleaks binary.
#[derive(Clone, Debug)]
pub struct Gitleaks {
    pub bin: PathBuf,
    pub home: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GitleaksFinding {
    #[serde(rename = "RuleID")]
    rule_id: String,
    file: String,
    start_line: u64,
    commit: String,
}

impl SecretScanner for Gitleaks {
    async fn scan(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, DeliveryError> {
        let cwd = scratch(&self.home, "gitleaks-")?;
        let log_opts = format!("--log-opts={range}");
        let out = tokio::process::Command::new(&self.bin)
            .current_dir(cwd.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "git",
                "--no-banner",
                "--no-color",
                "--log-level=error",
                "--redact",
                "--ignore-gitleaks-allow",
                "--exit-code=0",
                "--report-format=json",
                "--report-path=-",
                &log_opts,
            ])
            .arg(repo)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                DeliveryError::unavailable(format!("gitleaks {}: {e}", self.bin.display()))
            })?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "gitleaks exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let json = text.trim();
        let findings: Vec<GitleaksFinding> = if json.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(json)
                .map_err(|e| DeliveryError::unavailable(format!("gitleaks report: {e}")))?
        };
        Ok(findings
            .into_iter()
            .map(|f| Finding {
                rule_id: f.rule_id,
                file: f.file,
                line: f.start_line,
                commit: f.commit,
            })
            .collect())
    }
}

#[derive(Debug)]
pub struct Validator<B, S> {
    pub blobs: B,
    pub scanner: S,
    pub git: Git,
    pub bucket: String,
    pub work_root: PathBuf,
}

fn check_request(r: &ValidateRequest) -> Result<(), DeliveryError> {
    harness_ref(&r.run_id)?;
    check_head_sha(&r.head_sha)?;
    if let Some(b) = &r.base_sha {
        check_head_sha(b)?;
    }
    Ok(())
}

impl<B: Blobs, S: SecretScanner> Validator<B, S> {
    /// Writes the verified bundle from the source bundle; the inner error is the violation text.
    async fn build_verified(
        &self,
        work: &Path,
        r: &ValidateRequest,
        uri: &str,
    ) -> Result<Result<(), String>, DeliveryError> {
        let key = run_object_key(uri, &self.bucket, &r.run_id)?;
        let dir = work.join("source");
        let repo = dir.join("repo.git");
        self.git.init_bare(&repo).await?;
        let source = dir.join("source.bundle");
        if self.blobs.get_to_file(&key, &source).await?.is_none() {
            return Ok(Err(format!("no source bundle at {uri}")));
        }
        if let Err(e) = self.git.bundle_verify(&repo, &source).await {
            return Ok(Err(format!("the source bundle {uri} does not verify: {e}")));
        }
        self.git
            .fetch(&repo, &Remote::local(&source), &["+refs/*:refs/source/*"])
            .await?;
        if self.git.rev_parse(&repo, &r.head_sha).await?.as_deref() != Some(r.head_sha.as_str()) {
            return Ok(Err(format!(
                "headSha {} is not in the source bundle {uri}",
                r.head_sha
            )));
        }
        let hr = harness_ref(&r.run_id)?;
        self.git.update_ref(&repo, hr.as_str(), &r.head_sha).await?;
        let out = dir.join("verified.bundle");
        self.git.bundle_create(&repo, &out, &[hr.as_str()]).await?;
        let (sha256_hex, _) = sha256_file(&out)?;
        self.blobs
            .put_file(
                &verified_bundle_key(&r.run_id),
                &out,
                &PutMeta {
                    content_type: "application/octet-stream",
                    sha256_hex,
                },
            )
            .await?;
        Ok(Ok(()))
    }

    pub async fn validate(&self, r: &ValidateRequest) -> Result<ValidateOutput, DeliveryError> {
        check_request(r)?;
        if let Some(uri) = &r.source_bundle_uri {
            run_object_key(uri, &self.bucket, &r.run_id)?;
        }
        let done = |changed_files: u64,
                    violations: Vec<Violation>,
                    findings: Vec<Violation>,
                    verified: bool| ValidateOutput {
            pass: violations.is_empty(),
            run_id: r.run_id.clone(),
            base_sha: r.base_sha.clone(),
            head_sha: r.head_sha.clone(),
            changed_files,
            violations,
            findings,
            verified_bundle_uri: verified
                .then(|| format!("s3://{}/{}", self.bucket, verified_bundle_key(&r.run_id))),
        };
        let work = scratch(&self.work_root, "validate-")?;
        if let Some(uri) = &r.source_bundle_uri
            && let Err(detail) = self.build_verified(work.path(), r, uri).await?
        {
            return Ok(done(
                0,
                vec![violation(Rule::Bundle, None, detail)],
                Vec::new(),
                false,
            ));
        }
        let loaded = match load_verified(
            &self.git,
            &self.blobs,
            &self.bucket,
            work.path(),
            &r.run_id,
            &r.head_sha,
        )
        .await?
        {
            Ok(l) => l,
            Err(p) => {
                use crate::bundle::BundleProblem::{Missing, Verify};
                let rule = if matches!(p, Missing { .. } | Verify { .. }) {
                    Rule::Bundle
                } else {
                    Rule::Branch
                };
                return Ok(done(
                    0,
                    vec![violation(rule, None, p.to_string())],
                    Vec::new(),
                    false,
                ));
            }
        };
        let repo = loaded.repo.as_path();
        let mut violations = Vec::new();
        if loaded.size_bytes > r.limits.max_bundle_bytes {
            violations.push(violation(
                Rule::BundleSize,
                None,
                format!(
                    "the bundle is {} bytes; the limit is {} bytes",
                    loaded.size_bytes, r.limits.max_bundle_bytes
                ),
            ));
        }
        let base = match &r.base_sha {
            Some(b) => {
                if self.git.rev_parse(repo, b).await?.is_none() {
                    violations.push(violation(
                        Rule::History,
                        None,
                        format!("base {b} is not in the verified bundle"),
                    ));
                    return Ok(done(0, violations, Vec::new(), true));
                }
                if !self.git.is_ancestor(repo, b, &r.head_sha).await? {
                    violations.push(violation(
                        Rule::History,
                        None,
                        format!("head {} does not descend from base {b}", r.head_sha),
                    ));
                }
                b.as_str()
            }
            None => EMPTY_TREE,
        };
        let changes = self.git.diff_tree(repo, base, &r.head_sha).await?;
        let tree: BTreeMap<String, TreeEntry> = self
            .git
            .ls_tree(repo, &r.head_sha)
            .await?
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        violations.extend(evaluate_in(
            r.engine_config,
            &changes,
            &tree,
            &r.allowed_globs,
            &r.limits,
        ));
        let range = match &r.base_sha {
            Some(b) => format!("{b}..{}", r.head_sha),
            None => r.head_sha.clone(),
        };
        for f in self.scanner.scan(repo, &range).await? {
            let commit = &f.commit[..f.commit.len().min(12)];
            violations.push(violation(
                Rule::Secret,
                Some(&f.file),
                format!(
                    "gitleaks rule {} at {} line {} in commit {commit}",
                    f.rule_id, f.file, f.line
                ),
            ));
        }
        let copies = third_party::check(
            &self.blobs,
            &self.git,
            repo,
            work.path(),
            &r.run_id,
            r.base_sha.as_deref(),
            &r.head_sha,
            &tree,
            r.limits.max_file_bytes,
        )
        .await?;
        let mut findings = Vec::new();
        match r.third_party {
            ThirdPartyMode::Block => violations.extend(copies),
            ThirdPartyMode::Allow => findings = copies,
        }
        let out = done(changes.len() as u64, violations, findings, true);
        if out.pass {
            self.publish_changes(work.path(), r, &changes).await;
        }
        Ok(out)
    }

    /// Writes `runs/<run>/deliverable/changes.json` beside the verified bundle: the changes this validation checked, for
    /// the follow-up digest. It describes a delivery that passed and is an aid to the person's next prompt, so a write that fails is logged and
    /// never fails the delivery.
    async fn publish_changes(&self, work: &Path, r: &ValidateRequest, changes: &[Change]) {
        let bytes = crate::changes::record(&r.run_id, &r.head_sha, r.base_sha.as_deref(), changes);
        let file = work.join("changes.json");
        let Ok(()) = std::fs::write(&file, &bytes) else {
            return;
        };
        let Ok((sha256_hex, _)) = sha256_file(&file) else {
            return;
        };
        let meta = PutMeta {
            content_type: "application/json",
            sha256_hex,
        };
        if let Err(e) = self
            .blobs
            .put_file(&crate::changes::changes_key(&r.run_id), &file, &meta)
            .await
        {
            eprintln!(
                "bundle-validator: run {}: the change record was not written: {e}",
                r.run_id
            );
        }
    }
}
